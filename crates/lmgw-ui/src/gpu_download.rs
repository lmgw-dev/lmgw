//! The download for your graphics card (machine-check design §5, D14): a
//! model's engine image that is not on this machine yet, fetched before its
//! first start or together with a model download (D16).
//!
//! One rendering wherever a waiting start or a model download shows it —
//! the GPU popover, Traffic's GPU panel, the Overview, the Models list, the
//! labs, the Chat's tool card and its turn line, the download wizard and the
//! Downloads page: the sentence "Download for your graphics card · 1.2 of 3.4
//! GiB" (a chip says "graphics card download · 1.2 of 3.4 GiB"), a bar, and a
//! tooltip with the image reference and, when the size is not known, why.
//! The plain sentence never says "engine", "container" or "image"; the
//! reference is the detail.
//!
//! The bytes are as Podman reports them: about one update a second per layer
//! in flight, none for a layer that lands within a second, and a total that
//! may not be known (no registry manifest, or the `podman pull` command's
//! path, which counts no bytes). So the bar is determinate only with a
//! total; without one it is an indeterminate bar, never a made-up fraction.
//! Once every byte is in, Podman still unpacks the layers, which it does not
//! count: the line says "unpacking" then, and the bar is indeterminate again
//! rather than standing full.
//!
//! The state, its words and the bar are the kit's
//! (`lmgw_ui_kit::download`), shared with the Chat; the live sources and
//! who a download is for are here.

use leptos::prelude::*;
use lmgw_api_types::{EngineDownload, EngineFetch, JobRow, SettingsFull, VramStatus};

pub use lmgw_ui_kit::download::{Dl, DlBar, DlChip, DlLine, PULL_KIND, SENTENCE};

/// How often the wizard's line reads its job's row while the jobs feed does
/// not list it ([`PullLine`]).
const PULL_READ_MS: u64 = 2_000;

/// The download a start of `class`'s model `model_id` waits for, from the
/// vram view's `downloading` list.
pub fn for_model(v: &VramStatus, class: &str, model_id: &str) -> Option<Dl> {
    v.downloading
        .iter()
        .find(|w| w.container == class && w.model == model_id)
        .and_then(Dl::of_waiter)
}

/// The models whose start waits for a download, `(class, model, alias)`,
/// each once: two requests for one model wait for the same bytes.
pub fn waiting(v: &VramStatus) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for w in v.downloading.iter().filter(|w| Dl::of_waiter(w).is_some()) {
        if !out
            .iter()
            .any(|(c, m, _)| *c == w.container && *m == w.model)
        {
            out.push((w.container.clone(), w.model.clone(), w.alias.clone()));
        }
    }
    out
}

/// [`for_model`] off the live `vram` frame, as a signal a row keeps while
/// its bytes move: the row is drawn once, and the bar's animation runs on.
pub fn live_for(class: String, model_id: String) -> Signal<Option<Dl>> {
    let live = crate::live::use_live();
    Signal::derive(move || {
        live.vram
            .with(|v| v.as_ref().and_then(|v| for_model(v, &class, &model_id)))
    })
}

/// Every live `image_pull` job, with its id.
pub fn pulls(jobs: &[JobRow]) -> Vec<(i64, Dl)> {
    jobs.iter()
        .filter_map(|j| Dl::of_job(j).map(|d| (j.id, d)))
        .collect()
}

/// How many `image_pull` jobs run: what the Downloads page's header and
/// the sidebar's count add to the model files.
pub fn pull_count(jobs: &[JobRow]) -> usize {
    jobs.iter().filter(|j| j.kind == PULL_KIND).count()
}

/// Live job `id`'s download, off the jobs feed.
fn job_dl(id: i64) -> Memo<Option<Dl>> {
    let live = crate::live::use_live();
    Memo::new(move |_| {
        live.jobs.with(|j| {
            j.as_deref()
                .unwrap_or_default()
                .iter()
                .find(|j| j.id == id)
                .and_then(Dl::of_job)
        })
    })
}

/// `reference` as podman spells it fully: a bare name is Docker Hub's
/// (`docker.io/library/…`), and a name without a tag or digest is
/// `:latest`. Loose on purpose: it only matches a job's reference to the
/// setting that names it, for words.
fn qualified(reference: &str) -> String {
    let r = reference.trim();
    let r = r.strip_prefix("docker://").unwrap_or(r);
    let mut out = match r.split_once('/') {
        None => format!("docker.io/library/{r}"),
        Some((first, _)) if first.contains(['.', ':']) || first == "localhost" => r.to_string(),
        Some(_) => format!("docker.io/{r}"),
    };
    let last = out.rsplit('/').next().unwrap_or_default();
    if !last.contains(':') && !last.contains('@') {
        out.push_str(":latest");
    }
    out
}

/// The classes in the order the words list them, each with what its models
/// do in plain words.
const CLASS_WORDS: [(&str, &str); 4] = [
    ("chat", "chat models"),
    ("aux", "search models"),
    ("audio", "voice"),
    ("image", "pictures"),
];

/// Who a download of `image` is for, in plain words: the models whose start
/// waits for it by name ("for qwen-small"), else the classes whose Image
/// setting names it ("for chat models and search models"); `None` when
/// neither (an Engines-page pull of another image).
pub fn who_for(image: &str, starts: &[String], class_images: &[(&str, String)]) -> Option<String> {
    let image = qualified(image);
    let names: Vec<&str> = if !starts.is_empty() {
        starts.iter().map(String::as_str).collect()
    } else {
        CLASS_WORDS
            .iter()
            .filter(|(class, _)| {
                class_images
                    .iter()
                    .any(|(c, i)| c == class && !i.trim().is_empty() && qualified(i) == image)
            })
            .map(|(_, words)| *words)
            .collect()
    };
    match names.as_slice() {
        [] => None,
        [one] => Some(format!("for {one}")),
        [rest @ .., last] => Some(format!("for {} and {last}", rest.join(", "))),
    }
}

/// The class images as Settings → Runtimes names them.
fn class_images(s: &SettingsFull) -> Vec<(&'static str, String)> {
    vec![
        ("chat", s.router.image.clone()),
        ("aux", s.aux_router.image.clone()),
        ("audio", s.audio.image.clone()),
        ("image", s.image.image.clone()),
    ]
}

/// The names of the starts waiting for `image`: the alias each was asked
/// for, else the model.
fn starts_for(v: &VramStatus, image: &str) -> Vec<String> {
    let image = qualified(image);
    let mut out: Vec<String> = Vec::new();
    for w in &v.downloading {
        let Some(d) = Dl::of_waiter(w) else { continue };
        if qualified(&d.image) != image {
            continue;
        }
        let name = if w.alias.is_empty() {
            w.model.clone()
        } else {
            w.alias.clone()
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Every download for your graphics card running now — the live
/// `image_pull` jobs, whoever began them: a model download (D16), a start,
/// or an Engines-page pull. One line each, saying who it is for; nothing at
/// all when none runs.
#[component]
pub fn GpuPulls() -> impl IntoView {
    let live = crate::live::use_live();
    let ids = Memo::new(move |_| {
        live.jobs.with(|j| {
            pulls(j.as_deref().unwrap_or_default())
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        })
    });
    // Read once the first line shows: which class's Image names which
    // reference does not move while a download runs.
    let any = Memo::new(move |_| ids.with(|i| !i.is_empty()));
    let settings = LocalResource::new(move || {
        let any = any.get();
        async move {
            match any {
                true => crate::api::get::<SettingsFull>("/api/settings-full")
                    .await
                    .ok(),
                false => None,
            }
        }
    });
    let images = Memo::new(move |_| {
        settings
            .get()
            .flatten()
            .map(|s| class_images(&s))
            .unwrap_or_default()
    });
    view! {
        <Show when=move || any.get()>
            <div class="gpu-pulls">
                <For each=move || ids.get() key=|id| *id let:id>
                    {
                        let dl = job_dl(id);
                        let who = Memo::new(move |_| {
                            let image = dl.with(|d| d.as_ref().map(|d| d.image.clone()))?;
                            let starts = live.vram.with(|v| {
                                v.as_ref().map(|v| starts_for(v, &image)).unwrap_or_default()
                            });
                            images.with(|i| who_for(&image, &starts, i))
                        });
                        view! {
                            <DlLine
                                dl=Signal::derive(move || dl.get().unwrap_or_default())
                                who=Signal::derive(move || who.get())
                            />
                        }
                    }
                </For>
            </div>
        </Show>
    }
}

/// Where a model download's fetch of its class's image stands (D16), as the
/// download wizard says it ([`PullLine`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Pull {
    /// On the jobs feed: its bytes.
    Running(Dl),
    /// Queued or running by its row, and not on the feed: before the feed
    /// lists it, or after it left and before the row says how it ended.
    Waiting,
    Done,
    /// Failed or cancelled: why.
    Stopped(String),
    /// Its row could not be read: why.
    Unread(String),
    /// It could not begin: why.
    Refused(String),
    /// Whether it is needed at all was not known: why.
    Unchecked(String),
}

impl Pull {
    /// What the model download's answer said, before any read.
    pub fn of_engine(e: &EngineDownload) -> Self {
        let why = e.note.clone().unwrap_or_default();
        match (e.state, e.job_id) {
            (EngineFetch::NotKnown, _) => Self::Unchecked(why),
            (EngineFetch::Downloading, Some(_)) => Self::Waiting,
            (EngineFetch::Refused | EngineFetch::Downloading, _) => Self::Refused(why),
        }
    }

    /// The job's row says `status`, and `error` when it failed.
    pub fn of_status(status: &str, error: Option<String>) -> Self {
        match status {
            "queued" | "running" => Self::Waiting,
            "done" => Self::Done,
            other => Self::Stopped(error.unwrap_or_else(|| other.to_string())),
        }
    }

    /// Whether a read of its row may still change it.
    pub fn open(&self) -> bool {
        matches!(self, Self::Running(_) | Self::Waiting)
    }

    /// The line's words, in plain words that say what happens next.
    pub fn words(&self) -> String {
        match self {
            Self::Running(d) => d.words(),
            Self::Waiting => SENTENCE.to_string(),
            Self::Done => format!("{SENTENCE} · done"),
            Self::Stopped(_) => {
                format!("{SENTENCE} · stopped — it tries again the first time this model is used")
            }
            Self::Unread(_) => format!(
                "{SENTENCE} · could not tell how it ended — the first use of this model checks \
                 again"
            ),
            Self::Refused(_) => {
                format!("{SENTENCE} · can't be downloaded, so this model can't start yet")
            }
            Self::Unchecked(_) => format!(
                "{SENTENCE} · could not check whether it is needed — the first use of this \
                 model checks again"
            ),
        }
    }

    /// The tooltip after the reference: why, when there is one.
    pub fn why(&self) -> Option<&str> {
        match self {
            Self::Stopped(w) | Self::Unread(w) | Self::Refused(w) | Self::Unchecked(w) => {
                Some(w.as_str()).filter(|w| !w.is_empty())
            }
            _ => None,
        }
    }

    /// Amber: the model will not start until something is done about it, or
    /// its download has to run again.
    pub fn warns(&self) -> bool {
        matches!(self, Self::Stopped(_) | Self::Refused(_))
    }

    /// Its `data-gpu-download` value.
    pub fn attr(&self) -> &'static str {
        match self {
            Self::Running(_) | Self::Waiting => "running",
            Self::Done => "done",
            Self::Stopped(_) => "failed",
            Self::Unread(_) => "unread",
            Self::Refused(_) => "refused",
            Self::Unchecked(_) => "unchecked",
        }
    }
}

/// The download a model download began (D16): its line while it runs, and
/// how it ended once it left the jobs feed. While the feed does not list it
/// its row is read every [`PULL_READ_MS`], so a job that ended before the
/// feed ever listed it is said too. A pull refused before it began, or one
/// whose need was not known, is said, with why in its tooltip.
#[component]
pub fn PullLine(engine: EngineDownload) -> impl IntoView {
    let image = engine.image.clone();
    let first = Pull::of_engine(&engine);
    let Some(id) = engine.job_id.filter(|_| first == Pull::Waiting) else {
        return still_line(image, first).into_any();
    };
    let running = job_dl(id);
    // The row's word, once read: replaced by every later read.
    let read = RwSignal::new(None::<Pull>);
    let state = Memo::new(move |_| match running.get() {
        Some(d) => Pull::Running(d),
        None => read.get().unwrap_or(Pull::Waiting),
    });
    let ask = move || {
        leptos::task::spawn_local(async move {
            let p = match crate::backends_api::container_image_pull_status(id).await {
                Ok(s) => Pull::of_status(&s.status, s.error),
                Err(e) => Pull::Unread(e.to_string()),
            };
            read.try_set(Some(p));
        });
    };
    if let Ok(h) = set_interval_with_handle(
        move || {
            let off_feed = running.try_with_untracked(Option::is_none).unwrap_or(false);
            let open = state.try_with_untracked(Pull::open).unwrap_or(false);
            if off_feed && open {
                ask();
            }
        },
        std::time::Duration::from_millis(PULL_READ_MS),
    ) {
        on_cleanup(move || h.clear());
    }
    let on_feed = Memo::new(move |_| running.with(Option::is_some));
    view! {
        <Show
            when=move || on_feed.get()
            fallback=move || {
                let image = image.clone();
                move || still_line(image.clone(), state.get())
            }
        >
            <DlLine dl=Signal::derive(move || running.get().unwrap_or_default())/>
        </Show>
    }
    .into_any()
}

/// A [`Pull`] that has no bytes to show: its words, why in the tooltip.
fn still_line(image: String, p: Pull) -> impl IntoView {
    let title = match p.why() {
        Some(w) => format!("{image}\n{w}"),
        None => image,
    };
    let (warns, quiet) = (p.warns(), matches!(p, Pull::Unread(_) | Pull::Unchecked(_)));
    view! {
        <div class="dl-gpu" title=title data-gpu-download=p.attr()>
            <span class="dl-gpu-words" class:warn-text=warns class:dim=quiet>
                {p.words()}
            </span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::{VramWaiter, WaitPhase};

    #[test]
    fn a_pull_job_and_a_waiter_read_alike() {
        let job = JobRow {
            id: 4,
            kind: "image_pull".into(),
            key: Some("image:ghcr.io/ggml-org/llama.cpp:server-cuda".into()),
            label: "pull ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
            done: 1_200_000_000,
            total: Some(3_400_000_000),
            detail: serde_json::json!({"last_line": "", "done_bytes": 1_200_000_000u64,
                "total_bytes": 3_400_000_000u64}),
            ..Default::default()
        };
        let waiter = VramWaiter {
            model: "qwen".into(),
            container: "chat".into(),
            phase: Some(WaitPhase::Downloading {
                image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
                done_bytes: 1_200_000_000,
                total_bytes: Some(3_400_000_000),
                size_note: None,
            }),
            ..Default::default()
        };
        assert_eq!(Dl::of_job(&job), Dl::of_waiter(&waiter));
        let v = VramStatus {
            downloading: vec![waiter],
            ..Default::default()
        };
        assert!(for_model(&v, "chat", "qwen").is_some());
        assert!(for_model(&v, "aux", "qwen").is_none());
        assert_eq!(pulls(std::slice::from_ref(&job)).len(), 1);
        assert_eq!(pull_count(&[job]), 1);
    }

    /// Review M4: two lines never both say only the sentence — each says
    /// who it is for.
    #[test]
    fn a_pull_says_who_it_is_for() {
        let classes = vec![
            ("chat", "ghcr.io/ggml-org/llama.cpp:server-cuda".to_string()),
            ("aux", "ghcr.io/ggml-org/llama.cpp:server-cuda".to_string()),
            ("audio", "ghcr.io/0xshug0/audio.cpp:full-cuda12".to_string()),
            ("image", String::new()),
        ];
        assert_eq!(
            who_for("ghcr.io/ggml-org/llama.cpp:server-cuda", &[], &classes).as_deref(),
            Some("for chat models and search models")
        );
        assert_eq!(
            who_for("ghcr.io/0xshug0/audio.cpp:full-cuda12", &[], &classes).as_deref(),
            Some("for voice")
        );
        // A start waiting for it is named instead.
        assert_eq!(
            who_for(
                "ghcr.io/ggml-org/llama.cpp:server-cuda",
                &["qwen-small".to_string()],
                &classes
            )
            .as_deref(),
            Some("for qwen-small")
        );
        // A job's reference matches the setting's shorter spelling.
        let hub = vec![("image", "lmgw-test/sd:cuda".to_string())];
        assert_eq!(
            who_for("docker.io/lmgw-test/sd:cuda", &[], &hub).as_deref(),
            Some("for pictures")
        );
        assert_eq!(qualified("busybox"), "docker.io/library/busybox:latest");
        assert_eq!(
            qualified("localhost/lmgw-dev/x"),
            "localhost/lmgw-dev/x:latest"
        );
        assert_eq!(who_for("registry.example/other:1", &[], &classes), None);
        let three = vec![
            ("chat", "registry.example/e:1".to_string()),
            ("audio", "registry.example/e:1".to_string()),
            ("image", "registry.example/e:1".to_string()),
        ];
        assert_eq!(
            who_for("registry.example/e:1", &[], &three).as_deref(),
            Some("for chat models, voice and pictures")
        );
    }

    /// The wizard's line through its states (review L1, L3, L5): what the
    /// answer said, each row read, and the words for each, which say what
    /// happens next and never the reference.
    #[test]
    fn the_wizards_line_says_each_state_and_what_comes_next() {
        let e = |state, job_id, note: Option<&str>| EngineDownload {
            image: "registry.example/engine:cuda".into(),
            state,
            job_id,
            note: note.map(str::to_string),
        };
        assert_eq!(
            Pull::of_engine(&e(EngineFetch::Downloading, Some(3), None)),
            Pull::Waiting
        );
        let refused = Pull::of_engine(&e(EngineFetch::Refused, None, Some("built here")));
        assert_eq!(refused, Pull::Refused("built here".into()));
        assert!(refused.warns() && !refused.open());
        let unchecked = Pull::of_engine(&e(EngineFetch::NotKnown, None, Some("locked")));
        assert_eq!(unchecked, Pull::Unchecked("locked".into()));
        assert!(!unchecked.warns());
        assert_eq!(unchecked.why(), Some("locked"));

        // A row that still runs keeps the line open, so it is read again.
        assert!(Pull::of_status("queued", None).open());
        assert!(Pull::of_status("running", None).open());
        assert_eq!(Pull::of_status("done", None), Pull::Done);
        assert!(!Pull::Done.open());
        let failed = Pull::of_status("failed", Some("manifest unknown".into()));
        assert_eq!(failed, Pull::Stopped("manifest unknown".into()));
        assert_eq!(
            Pull::of_status("canceled", None),
            Pull::Stopped("canceled".into())
        );
        assert!(failed.warns() && !failed.open());

        assert_eq!(Pull::Done.words(), "Download for your graphics card · done");
        assert_eq!(
            failed.words(),
            "Download for your graphics card · stopped — it tries again the first time this \
             model is used"
        );
        assert_eq!(
            refused.words(),
            "Download for your graphics card · can't be downloaded, so this model can't start \
             yet"
        );
        for p in [
            Pull::Waiting,
            Pull::Done,
            failed,
            refused,
            unchecked,
            Pull::Unread("x".into()),
        ] {
            let w = p.words();
            assert!(w.starts_with(SENTENCE), "{w}");
            for word in ["engine", "container", "image", "first start"] {
                assert!(!w.contains(word), "{word} in {w}");
            }
        }
    }
}
