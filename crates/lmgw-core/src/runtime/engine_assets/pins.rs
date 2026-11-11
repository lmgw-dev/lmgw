//! The pin table (Windows design §4.2): one entry per engine build, read
//! from `pins.json` beside this file and compiled into lmgw.
//!
//! Per platform and engine: the upstream repository, the release tag, and
//! per variant the asset's name, SHA-256 (what the GitHub API reports as
//! `digest`) and size, the cudart companion of a CUDA build, and that
//! build's CUDA coverage — every file with device code, its SASS and PTX
//! architectures as `cuobjdump --list-elf --list-ptx` lists them. The
//! coverage is a recorded fact: `scripts/engine-cuda-coverage.sh --check
//! <new dir>` reads it again from the assets and compares.
//!
//! eSpeak NG (the phonemizing audio.cpp families need it, §2 point 4) has an
//! entry of its own: one MSI, whose digest GitHub does not report (it was
//! uploaded before GitHub kept digests), so the pinned SHA-256 is the one of
//! the download, taken 2026-11-10 on two machines.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// The table as committed.
const PINS_JSON: &str = include_str!("pins.json");

/// A build's variant: what it computes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Cuda13,
    Cuda12,
    Vulkan,
    Cpu,
}

impl Variant {
    /// The order the choice tries them in (§4.2).
    pub const PREFERENCE: [Variant; 4] = [Self::Cuda13, Self::Cuda12, Self::Vulkan, Self::Cpu];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cuda13 => "cuda13",
            Self::Cuda12 => "cuda12",
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }

    pub fn is_cuda(self) -> bool {
        matches!(self, Self::Cuda13 | Self::Cuda12)
    }

    /// The CUDA major version a CUDA build is made with.
    pub fn cuda_major(self) -> Option<u32> {
        match self {
            Self::Cuda13 => Some(13),
            Self::Cuda12 => Some(12),
            _ => None,
        }
    }
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One release asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    /// Lower-case hex, no `sha256:`.
    pub sha256: String,
    /// Bytes.
    pub size: u64,
}

/// One file of a CUDA build that carries device code, as `cuobjdump` lists
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CudaFile {
    /// Its path inside the build (`ggml-cuda.dll`).
    pub file: String,
    /// The SASS architectures (`86` for `sm_86`, `120a` for `sm_120a`).
    pub sass: Vec<String>,
    /// The PTX architectures (`compute_<n>`), which the driver JIT-compiles
    /// for a newer card.
    pub ptx: Vec<String>,
    /// The PTX's ISA version (`.version 8.4`), when it carries PTX.
    pub ptx_isa: Option<String>,
}

/// A CUDA build's toolkit and coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CudaPin {
    /// The CUDA toolkit it is built with (`12.4`): its cudart and the
    /// newest driver API its PTX needs.
    pub toolkit: String,
    pub files: Vec<CudaFile>,
}

/// One build of one engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPin {
    pub variant: Variant,
    pub asset: Asset,
    /// The CUDA runtime DLLs, a separate asset upstream, unpacked into the
    /// same directory.
    #[serde(default)]
    pub cudart: Option<Asset>,
    #[serde(default)]
    pub cuda: Option<CudaPin>,
}

impl BuildPin {
    /// The assets unpacked into the build's directory, in order.
    pub fn assets(&self) -> impl Iterator<Item = &Asset> {
        std::iter::once(&self.asset).chain(self.cudart.as_ref())
    }

    /// Bytes to download.
    pub fn download_size(&self) -> u64 {
        self.assets().map(|a| a.size).sum()
    }
}

/// One engine's pinned release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnginePins {
    /// `owner/name` on GitHub.
    pub repo: String,
    pub tag: String,
    /// The release's publication day (`2026-11-10`).
    pub published: String,
    pub builds: Vec<BuildPin>,
}

impl EnginePins {
    pub fn build(&self, variant: Variant) -> Option<&BuildPin> {
        self.builds.iter().find(|b| b.variant == variant)
    }

    /// The build directory's name: `<tag>-<variant>` (§4.2).
    pub fn dir_name(&self, variant: Variant) -> String {
        format!("{}-{}", self.tag, variant.as_str())
    }
}

/// A single-asset pin (eSpeak NG).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinglePin {
    pub repo: String,
    pub tag: String,
    pub published: String,
    pub asset: Asset,
}

/// Every pin of one platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPins {
    pub llama: EnginePins,
    pub sdcpp: EnginePins,
    pub audio: EnginePins,
    pub espeak: SinglePin,
}

impl PlatformPins {
    /// The pins of an engine by its name ([`crate::runtime::Class::engine`]).
    pub fn engine(&self, engine: &str) -> Option<&EnginePins> {
        match engine {
            "llama" => Some(&self.llama),
            "sdcpp" => Some(&self.sdcpp),
            "audio" => Some(&self.audio),
            _ => None,
        }
    }

    /// `(engine, pins)` for the three engines.
    pub fn engines(&self) -> [(&'static str, &EnginePins); 3] {
        [
            ("llama", &self.llama),
            ("sdcpp", &self.sdcpp),
            ("audio", &self.audio),
        ]
    }
}

/// The table: platform (`windows-x86_64`) → its pins.
pub type Table = BTreeMap<String, PlatformPins>;

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    // The table is a committed file a test parses and checks
    // (`the_committed_table_parses_and_is_complete`), so a broken one never
    // reaches a build that ships.
    serde_json::from_str(PINS_JSON).expect("pins.json is checked by its test")
});

/// The whole table.
pub fn table() -> &'static Table {
    &TABLE
}

/// This build's platform key: `<os>-<arch>` (`windows-x86_64`).
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The pins of the platform lmgw runs on, if the table has any (Windows on
/// x86_64 only, so far).
pub fn current() -> Option<&'static PlatformPins> {
    table().get(&platform())
}

/// The table's own checks: digests are 64 hex digits, every engine has a
/// CPU build, every CUDA build has a cudart companion and recorded coverage.
pub fn check(table: &Table) -> Result<(), String> {
    let hex = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    for (platform, p) in table {
        if !hex(&p.espeak.asset.sha256) {
            return Err(format!(
                "{platform} espeak: the SHA-256 is not 64 lower-case hex digits"
            ));
        }
        for (engine, e) in p.engines() {
            if e.build(Variant::Cpu).is_none() {
                return Err(format!("{platform} {engine}: no cpu build"));
            }
            for b in &e.builds {
                let at = format!("{platform} {engine} {}", b.variant);
                if e.builds.iter().filter(|o| o.variant == b.variant).count() > 1 {
                    return Err(format!("{at}: pinned twice"));
                }
                for a in b.assets() {
                    if !hex(&a.sha256) || a.size == 0 || a.name.is_empty() {
                        return Err(format!("{at}: asset {} has no digest or size", a.name));
                    }
                }
                match (b.variant.is_cuda(), &b.cudart, &b.cuda) {
                    (true, Some(_), Some(c)) if !c.files.is_empty() => {}
                    (true, _, _) => {
                        return Err(format!(
                            "{at}: a CUDA build needs its cudart asset and its recorded coverage"
                        ))
                    }
                    (false, None, None) => {}
                    (false, _, _) => {
                        return Err(format!("{at}: cudart or coverage on a non-CUDA build"))
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_table_parses_and_is_complete() {
        let t: Table = serde_json::from_str(PINS_JSON).unwrap();
        check(&t).unwrap();
        let w = t.get("windows-x86_64").expect("the Windows pins");
        assert_eq!(
            w.sdcpp.build(Variant::Cuda13),
            None,
            "sd.cpp ships no CUDA 13 build"
        );
        assert_eq!(
            w.llama.dir_name(Variant::Cuda12),
            format!("{}-cuda12", w.llama.tag)
        );
    }
}
