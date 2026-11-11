//! Engines as release assets (Windows design §4.2, WP4): where a native
//! engine comes from on a platform without Podman.
//!
//! - [`pins`] — the pin table: per engine one release, per variant its asset
//!   with SHA-256 and size, the cudart companion and the recorded CUDA
//!   coverage; eSpeak NG's MSI.
//! - [`coverage`] and [`choice`] — which variant this machine runs, per
//!   engine, from [`facts`] (NVML's cards and driver, the Vulkan loader).
//! - [`install`] — download, verify, unpack into
//!   `<engines>/<engine>/<tag>-<variant>/`, never run from the download
//!   directory; [`msi`] and [`cab`] unpack eSpeak NG's package.
//! - [`fallback`] — a CUDA build that ended a start with "no kernel image is
//!   available" is marked, and its engine runs its Vulkan build from then on.
//! - [`releases`] — the upstream release check behind the Engines page's
//!   "update available"; [`view`] — that page's Windows column
//!   (`engine_releases`).
//!
//! [`Engines`] is what the native launcher asks: the build of a class's
//! engine for this machine, installed. A platform the table has no pins for
//! (Linux, macOS today) keeps the hand-installed layout WP3 reads
//! ([`crate::runtime::launcher::native::engines`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod cab;
pub mod choice;
pub mod coverage;
pub mod facts;
pub mod fallback;
pub mod install;
pub mod msi;
pub mod pins;
pub mod releases;
pub mod view;

use choice::{Choice, GpuFacts};
use install::{Installer, Job, Record, Unpack};
use pins::{EnginePins, PlatformPins, Variant};

use crate::runtime::launcher::StartWatch;
use crate::runtime::Class;

/// The engines directory's name under the data dir.
pub const ENGINES_DIR: &str = "engines";

/// The engines root: **the one accessor** every engine path is made from
/// (the native launcher, the installer, the Engines page, the Windows
/// machine check's room): `<local dir>/engines`, the local dir being
/// `state::local_data_dir` of the data dir lmgw runs with (Windows design
/// §4.4: `%LOCALAPPDATA%\lmgw` for the installed app on Windows, the data
/// dir itself everywhere else).
pub fn engines_root(data_dir: &Path) -> PathBuf {
    crate::state::local_data_dir(data_dir).join(ENGINES_DIR)
}

/// eSpeak NG's directory under the engines root.
pub const ESPEAK_DIR: &str = "espeak";

/// The upstream project's name, for sentences.
pub fn engine_label(engine: &str) -> &'static str {
    match engine {
        "llama" => "llama.cpp",
        "sdcpp" => "stable-diffusion.cpp",
        "audio" => "audio.cpp",
        _ => "engine",
    }
}

/// The executable a class's engine is started as, with the platform's
/// suffix (`llama-server.exe` on Windows).
pub fn executable(class: Class) -> String {
    format!("{}{}", executable_stem(class), std::env::consts::EXE_SUFFIX)
}

/// [`executable`] without the suffix.
pub fn executable_stem(class: Class) -> &'static str {
    match class {
        Class::Chat | Class::Aux => "llama-server",
        Class::Image => "sd-server",
        Class::Audio => "audiocpp_server",
    }
}

/// One pinned build, chosen and installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub dir: PathBuf,
    pub variant: Variant,
    /// `<tag>-<variant>`.
    pub build: String,
    /// The choice's sentence.
    pub why: String,
    /// The first start JIT-compiles its kernels.
    pub jit: bool,
}

/// Where the facts come from: the machine, or a test's.
pub type FactsSource = Arc<dyn Fn() -> GpuFacts + Send + Sync>;

/// The pinned engines of one engines root.
pub struct Engines {
    installer: Installer,
    pins: Option<PlatformPins>,
    facts: FactsSource,
    /// Variants the caller chose instead of the machine, per engine (the
    /// live fallback test runs a CUDA build the coverage rules out).
    forced: HashMap<String, Variant>,
    /// eSpeak NG's last failed install, when and why: the audio starts
    /// within [`ESPEAK_RETRY_AFTER`] of it start without it at once.
    espeak_failed: Mutex<Option<(Instant, String)>>,
    /// The facts [`Self::facts`] last read: what [`Self::expected_build`]
    /// chooses by without reading the machine again (every start reads them
    /// afresh and refreshes this).
    last_facts: Mutex<Option<GpuFacts>>,
}

/// How long a failed eSpeak NG install is remembered: the audio starts in
/// that time do not try it again (with no network each try can cost the
/// connect timeout), and say so in their warning. The first audio start
/// after it, or after lmgw restarts, tries again.
pub const ESPEAK_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

impl Engines {
    /// This platform's pins ([`pins::current`]), downloaded from GitHub,
    /// the machine's facts.
    pub fn new(root: PathBuf) -> Self {
        Self::with(
            root,
            install::GITHUB,
            pins::current().cloned(),
            Arc::new(facts::read),
        )
    }

    /// Any pins, any download base, any facts (the tests).
    pub fn with(root: PathBuf, base: &str, pins: Option<PlatformPins>, facts: FactsSource) -> Self {
        Self {
            installer: Installer::new(root, base),
            pins,
            facts,
            forced: HashMap::new(),
            espeak_failed: Mutex::new(None),
            last_facts: Mutex::new(None),
        }
    }

    /// No pins: the hand-installed builds under `root` (the native launcher
    /// on a platform without pins, and the tests that install a build by
    /// hand on any).
    pub fn hand_installed(root: PathBuf) -> Self {
        Self::with(root, install::GITHUB, None, Arc::new(GpuFacts::default))
    }

    /// Run `variant` of `engine` whatever the machine says (it must be
    /// pinned), until a start of it ends without a kernel image
    /// ([`fallback`]). The live fallback check uses it; no setting reaches it
    /// yet.
    pub fn force_variant(mut self, engine: &str, variant: Variant) -> Self {
        self.forced.insert(engine.to_string(), variant);
        self
    }

    pub fn root(&self) -> &Path {
        self.installer.root()
    }

    pub fn pins(&self) -> Option<&PlatformPins> {
        self.pins.as_ref()
    }

    /// The pins of `class`'s engine, if this platform has any.
    pub fn engine_pins(&self, class: Class) -> Option<&EnginePins> {
        self.pins.as_ref()?.engine(class.engine())
    }

    /// The machine's facts now (blocking work off the runtime's threads).
    pub async fn facts(&self) -> GpuFacts {
        let f = self.facts.clone();
        let facts = tokio::task::spawn_blocking(move || f())
            .await
            .unwrap_or_default();
        *self.last_facts.lock().unwrap_or_else(|e| e.into_inner()) = Some(facts.clone());
        facts
    }

    /// The build of `class`'s engine the next start would run, by the facts
    /// last read and the fallback marks on disk now (no download, no read of
    /// the machine): `<tag>-<variant>` for a pinned engine, the one
    /// hand-installed build's directory name elsewhere. Before any facts were
    /// read, a name no build has, so nothing learned under a build counts
    /// yet. `None` when no build is installed by hand.
    pub fn expected_build(&self, class: Class) -> Option<String> {
        let Some(pins) = self.engine_pins(class) else {
            return crate::runtime::launcher::native::engines::scan(self.root(), class)
                .ok()
                .map(|i| i.build);
        };
        let facts = self
            .last_facts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some(match facts {
            Some(facts) => pins.dir_name(self.choose(class.engine(), pins, &facts).variant),
            None => format!("{}-not-chosen-yet", pins.tag),
        })
    }

    /// The build `engine` runs on this machine (no download).
    pub fn choose(&self, engine: &str, pins: &EnginePins, facts: &GpuFacts) -> Choice {
        let root = self.root().join(engine);
        let forced = self.forced.get(engine).filter(|v| {
            pins.build(**v).is_some() && fallback::marked(&root.join(pins.dir_name(**v))).is_none()
        });
        if let Some(v) = forced {
            return Choice {
                variant: *v,
                why: format!(
                    "{} {v}: chosen by the caller, not by this machine's facts",
                    pins.tag
                ),
                jit: false,
            };
        }
        choose_here(&root, pins, facts)
    }

    /// The directory a build is installed in.
    pub fn build_dir(&self, engine: &str, pins: &EnginePins, variant: Variant) -> PathBuf {
        self.root().join(engine).join(pins.dir_name(variant))
    }

    /// What a verified install of `variant` records.
    pub fn record(pins: &EnginePins, variant: Variant) -> Option<Record> {
        let b = pins.build(variant)?;
        Some(Record {
            repo: pins.repo.clone(),
            tag: pins.tag.clone(),
            variant: Some(variant),
            assets: b.assets().cloned().collect(),
            installed_at: String::new(),
        })
    }

    /// `variant` of `class`'s engine is installed and verified.
    pub fn is_installed(&self, engine: &str, pins: &EnginePins, variant: Variant) -> bool {
        Self::record(pins, variant)
            .is_some_and(|r| Installer::is_installed(&self.build_dir(engine, pins, variant), &r))
    }

    /// The build of `class`'s engine for this machine, installed now when
    /// it is not yet, for the start `watch` speaks for. `Ok(None)` when this
    /// platform has no pins for it.
    pub async fn ensure(
        &self,
        class: Class,
        watch: &dyn StartWatch,
    ) -> Result<Option<Installed>, String> {
        let engine = class.engine();
        let Some(pins) = self.engine_pins(class) else {
            return Ok(None);
        };
        let facts = self.facts().await;
        let choice = self.choose(engine, pins, &facts);
        self.install(class, pins, choice, watch).await.map(Some)
    }

    /// Install `choice` of `class`'s engine for the start `watch` speaks for.
    pub async fn install(
        &self,
        class: Class,
        pins: &EnginePins,
        choice: Choice,
        watch: &dyn StartWatch,
    ) -> Result<Installed, String> {
        let engine = class.engine();
        let variant = choice.variant;
        let record = Self::record(pins, variant)
            .ok_or_else(|| format!("{} {variant} is not pinned", engine_label(engine)))?;
        let exe = executable(class);
        let check = move |dir: &Path| -> Result<(), String> {
            if dir.join(&exe).is_file() {
                Ok(())
            } else {
                Err(format!(
                    "the unpacked release has no {exe} at its top level, where lmgw starts it \
                     (the upstream asset's layout changed)"
                ))
            }
        };
        let dir = self
            .installer
            .ensure(Job {
                dir: self.build_dir(engine, pins, variant),
                record,
                unpack: Unpack::Zip,
                label: format!("{} {} {variant}", engine_label(engine), pins.tag),
                check: &check,
                watch,
            })
            .await?;
        Ok(Installed {
            dir,
            variant,
            build: pins.dir_name(variant),
            why: choice.why,
            jit: choice.jit,
        })
    }

    /// Clear the "no kernel image" mark of `variant` of `engine`
    /// ([`fallback::clear`]): its next start may choose that build again.
    /// The sentence that says what was cleared, or why nothing was.
    pub fn clear_fallback(&self, engine: &str, variant: &str) -> Result<String, String> {
        let pins = self
            .pins
            .as_ref()
            .ok_or("this platform has no pinned engine builds, so no build is marked")?;
        let p = pins.engine(engine).ok_or_else(|| {
            format!("no pinned engine is called {engine:?} (llama, sdcpp, audio)")
        })?;
        let v: Variant = serde_json::from_value(serde_json::json!(variant))
            .map_err(|_| format!("{variant:?} is no variant (cuda13, cuda12, vulkan, cpu)"))?;
        if p.build(v).is_none() {
            return Err(format!(
                "{} {} has no {v} build",
                engine_label(engine),
                p.tag
            ));
        }
        let dir = self.build_dir(engine, p, v);
        let mark = fallback::marked(&dir);
        if !fallback::clear(&dir)? {
            return Err(format!(
                "{} {} {v} has no \"no kernel image\" mark ({})",
                engine_label(engine),
                p.tag,
                dir.display()
            ));
        }
        let said = mark.map(|m| m.line).unwrap_or_default();
        let sentence = format!(
            "cleared the \"no kernel image\" mark of {} {} {v} ({said}): the next start of the \
             engine chooses its build from this machine's facts again",
            engine_label(engine),
            p.tag
        );
        tracing::info!("{sentence}");
        Ok(sentence)
    }

    /// eSpeak NG, installed now when it is not yet, for the start `watch`
    /// speaks for: the directory to put on the audio engine's `PATH` and in
    /// its `ESPEAK_DATA_PATH` (it holds `libespeak-ng.dll` and
    /// `espeak-ng-data`). `Ok(None)` without pins. A failed install is
    /// remembered for [`ESPEAK_RETRY_AFTER`]: the error says so, and until
    /// then it is the answer at once.
    pub async fn ensure_espeak(&self, watch: &dyn StartWatch) -> Result<Option<PathBuf>, String> {
        let Some(pin) = self.pins.as_ref().map(|p| &p.espeak) else {
            return Ok(None);
        };
        let retry = format!(
            "lmgw tries the install again at the first audio start {} min after the failure, or \
             after lmgw restarts",
            ESPEAK_RETRY_AFTER.as_secs() / 60
        );
        if let Some((at, e)) = self
            .espeak_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(at, _)| at.elapsed() < ESPEAK_RETRY_AFTER)
        {
            return Err(format!(
                "{e}; not tried again, it failed {} s ago and {retry}",
                at.elapsed().as_secs()
            ));
        }
        let check = |dir: &Path| espeak_home(dir).map(|_| ());
        let installed = self
            .installer
            .ensure(Job {
                dir: self.root().join(ESPEAK_DIR).join(&pin.tag),
                record: Record {
                    repo: pin.repo.clone(),
                    tag: pin.tag.clone(),
                    variant: None,
                    assets: vec![pin.asset.clone()],
                    installed_at: String::new(),
                },
                unpack: Unpack::Msi,
                label: format!("eSpeak NG {}", pin.tag),
                check: &check,
                watch,
            })
            .await
            .and_then(|dir| espeak_home(&dir));
        let mut failed = self.espeak_failed.lock().unwrap_or_else(|e| e.into_inner());
        match installed {
            Ok(home) => {
                *failed = None;
                Ok(Some(home))
            }
            // A start that was stopped meanwhile says nothing about eSpeak.
            Err(e) if !watch.wanted() => Err(e),
            Err(e) => {
                *failed = Some((Instant::now(), e.clone()));
                Err(format!("{e}; {retry}"))
            }
        }
    }
}

/// The variant choice against the fallback marks under `engine_root`
/// (`<engines>/<engine>`).
pub fn choose_here(engine_root: &Path, pins: &EnginePins, facts: &GpuFacts) -> Choice {
    let fell_back = |v: Variant| fallback::marked(&engine_root.join(pins.dir_name(v))).is_some();
    choice::choose(pins, facts, &fell_back)
}

/// The directory under an unpacked eSpeak NG package that holds both its
/// library and its `espeak-ng-data` (the MSI puts them in its program
/// directory).
pub fn espeak_home(unpacked: &Path) -> Result<PathBuf, String> {
    let mut stack = vec![unpacked.to_path_buf()];
    while let Some(d) = stack.pop() {
        let lib = ["libespeak-ng.dll", "espeak-ng.dll"]
            .iter()
            .any(|n| d.join(n).is_file());
        if lib && d.join("espeak-ng-data").is_dir() {
            return Ok(d);
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            stack.extend(
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_dir()),
            );
        }
    }
    Err(format!(
        "{} holds no directory with libespeak-ng.dll and espeak-ng-data (the upstream package's \
         layout changed)",
        unpacked.display()
    ))
}
