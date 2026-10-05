//! The audio editor's "Runs on" and "Threads" fields (the per-row CPU
//! switch): a row switched to the CPU claims no VRAM, is never evicted,
//! keeps serving while the GPU hold is on and learns no residency.
//!
//! Blank inherits, as `lazy` does: the class's backend, and for the thread
//! count this machine's physical cores for a row switched to the CPU here,
//! else the class's figure (a row inheriting a class backend of `cpu`
//! included) — the placeholder says which, with the figure. Nothing is
//! capped or split here: several CPU rows busy at once each run their own
//! pool.

use leptos::prelude::*;
use lmgw_api_types::{AudioModel, AudioSettings, HostCpu, SettingsFull};
use serde_json::{json, Value};

use crate::widgets::Select;

/// The two fields' drafts.
#[derive(Clone, Copy)]
pub(super) struct RowPlacement {
    /// `""` inherits the class, `"cpu"` runs the row on the CPU.
    pub backend: RwSignal<String>,
    pub threads: RwSignal<String>,
}

impl RowPlacement {
    pub fn new(m: &AudioModel) -> Self {
        Self {
            backend: RwSignal::new(m.backend.clone().unwrap_or_default()),
            threads: RwSignal::new(m.threads.map(|n| n.to_string()).unwrap_or_default()),
        }
    }

    /// The patch's `backend` and `threads`, and the names a blank puts in
    /// `clear` (inherit).
    pub fn body(&self) -> Result<(Value, Value, Vec<&'static str>), String> {
        let backend = self.backend.get_untracked();
        let threads = parse_threads(&self.threads.get_untracked())?;
        let mut clear = Vec::new();
        if backend.is_empty() {
            clear.push("backend");
        }
        if threads.is_none() {
            clear.push("threads");
        }
        Ok((
            if backend.is_empty() {
                Value::Null
            } else {
                json!(backend)
            },
            threads.map_or(Value::Null, |n| json!(n)),
            clear,
        ))
    }

    /// The row runs on the CPU as drafted: its own `cpu`, or a class whose
    /// backend is `cpu`.
    fn on_cpu(&self, class: Option<&AudioSettings>) -> bool {
        on_cpu(&self.backend.get(), class.map(|c| c.backend.as_str()))
    }
}

/// `backend` in effect is `cpu`: the row's own, else the class's — matched
/// exactly, as audio.cpp and the gateway match it.
fn on_cpu(row: &str, class: Option<&str>) -> bool {
    match row.trim() {
        "" => class == Some("cpu"),
        r => r == "cpu",
    }
}

/// A thread count as typed: blank inherits, otherwise a whole number of 1 or
/// more (the gateway says so when it is above this machine's CPUs).
fn parse_threads(raw: &str) -> Result<Option<i64>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!(
            "threads: '{raw}' is not a thread count — a whole number of 1 or more, or blank to \
             inherit"
        )),
    }
}

/// The "Runs on" choices, the class default named by its backend.
fn runs_on_options(class_backend: Option<&str>) -> Vec<(String, String)> {
    let default = match class_backend {
        Some("cpu") => "Class default: CPU".to_string(),
        Some(b) if !b.is_empty() => format!("Class default: GPU ({b})"),
        _ => "Class default".to_string(),
    };
    vec![(String::new(), default), ("cpu".into(), "CPU".into())]
}

/// What a blank thread count runs with, and where that comes from — the
/// gateway's rule (`runtime::audio::threads_in_effect`): the physical cores
/// for a row switched to the CPU itself, else the class's figure.
fn threads_placeholder(row_backend: &str, s: Option<&SettingsFull>) -> String {
    let Some(s) = s else {
        return String::new();
    };
    if row_backend.trim() == "cpu" {
        format!(
            "{} · this machine's {}",
            s.host_cpu.physical_cores,
            cores_word(&s.host_cpu)
        )
    } else {
        format!("{} · audio class", s.audio.threads)
    }
}

/// How the host figure is named.
fn cores_word(h: &HostCpu) -> &'static str {
    match h.source.as_str() {
        "logical" => "logical CPUs (the core topology could not be read)",
        _ => "physical cores",
    }
}

/// The run args a blank override inherits, as the placeholder shows them: a
/// row on the CPU runs without the class's GPU passthrough.
pub(super) fn inherited_args(p: RowPlacement, s: &SettingsFull) -> String {
    let args = if p.on_cpu(Some(&s.audio)) {
        lmgw_api_types::audio_engine::cpu_run_args(&s.audio.extra_run_args)
    } else {
        s.audio.extra_run_args.clone()
    };
    args.join("\n")
}

/// The drafted override (one arg per line, as the field takes it) still
/// hands the container a GPU while the row runs on the CPU.
fn own_args_pass_gpu(p: RowPlacement, own: &str, class: Option<&AudioSettings>) -> bool {
    let own: Vec<String> = own
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    p.on_cpu(class) && lmgw_api_types::audio_engine::passes_gpu(&own)
}

/// The warning under the run-args override of a CPU row whose own args
/// still pass a GPU: they are used as written.
#[component]
pub(super) fn CpuGpuArgsNote(
    p: RowPlacement,
    own: RwSignal<String>,
    #[prop(into)] settings: Signal<Option<SettingsFull>>,
) -> impl IntoView {
    move || {
        settings
            .with(|s| own_args_pass_gpu(p, &own.get(), s.as_ref().map(|s| &s.audio)))
            .then(|| {
                view! {
                    <div class="notice warn">
                        "This row runs on the CPU, but these args still pass the GPU to its "
                        "container. They are used as written, so nothing keeps audio.cpp off the "
                        "card: remove those flags, or clear the override to inherit the class "
                        "args without them."
                    </div>
                }
            })
    }
}

/// The two fields, for the editor's Engine grid.
#[component]
pub(super) fn RunsOnFields(
    p: RowPlacement,
    #[prop(into)] settings: Signal<Option<SettingsFull>>,
) -> impl IntoView {
    let options = Signal::derive(move || {
        runs_on_options(
            settings
                .with(|s| s.as_ref().map(|s| s.audio.backend.clone()))
                .as_deref(),
        )
    });
    let placeholder = move || settings.with(|s| threads_placeholder(&p.backend.get(), s.as_ref()));
    view! {
        <div class="field">
            <label>"Runs on"</label>
            <Select value=p.backend options=options/>
        </div>
        <div class="field">
            <label>"Threads" <span class="field-unit">"blank inherits"</span></label>
            <input class="input mono"
                placeholder=placeholder
                prop:value=move || p.threads.get()
                on:input=move |ev| p.threads.set(event_target_value(&ev))/>
        </div>
    }
}

/// The Engine section's note on the two fields: what the CPU means, and how
/// several CPU rows share the cores.
#[component]
pub(super) fn RunsOnNote() -> impl IntoView {
    view! {
        <p class="dim mini-note">
            "On the CPU the row takes no GPU memory: it is never evicted, keeps serving while the "
            "GPU hold is on, and learns no residency; its inherited run args lose the GPU "
            "passthrough. Each CPU row runs its own pool of threads, so two busy at once share the "
            "cores — set a count per row when several run (and pin it with --cpuset-cpus if you "
            "like: a blank count still uses every physical core)."
        </p>
    }
}

/// The hold-fallback note for a row on the CPU, which the hold never stops.
#[component]
pub(super) fn CpuHoldNote(
    p: RowPlacement,
    #[prop(into)] settings: Signal<Option<SettingsFull>>,
) -> impl IntoView {
    move || {
        settings
            .with(|s| p.on_cpu(s.as_ref().map(|s| &s.audio)))
            .then(|| {
                view! {
                    <p class="dim mini-note">
                        "Not used while this row runs on the CPU: the GPU hold does not stop a "
                        "CPU row, so it keeps answering itself."
                    </p>
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(backend: &str, threads: i64, cores: u32, source: &str) -> SettingsFull {
        SettingsFull {
            audio: AudioSettings {
                backend: backend.into(),
                threads,
                extra_run_args: [
                    "--device",
                    "nvidia.com/gpu=all",
                    "--security-opt",
                    "label=disable",
                ]
                .map(String::from)
                .to_vec(),
                ..Default::default()
            },
            host_cpu: HostCpu {
                physical_cores: cores,
                logical_cpus: cores * 2,
                source: source.into(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_blank_thread_count_says_what_it_inherits() {
        let s = settings("cuda", 4, 16, "topology");
        assert_eq!(
            threads_placeholder("cpu", Some(&s)),
            "16 · this machine's physical cores"
        );
        assert_eq!(threads_placeholder("", Some(&s)), "4 · audio class");
        let odd = settings("cuda", 4, 32, "logical");
        assert!(threads_placeholder("cpu", Some(&odd)).contains("could not be read"));
        assert_eq!(threads_placeholder("cpu", None), "");
    }

    #[test]
    fn the_class_default_is_named_by_its_backend() {
        assert_eq!(
            runs_on_options(Some("cuda"))[0].1,
            "Class default: GPU (cuda)"
        );
        assert_eq!(runs_on_options(Some("cpu"))[0].1, "Class default: CPU");
        assert_eq!(runs_on_options(None)[0].1, "Class default");
        assert_eq!(runs_on_options(Some("cuda"))[1].0, "cpu");
    }

    #[test]
    fn a_thread_count_is_whole_and_positive_or_blank() {
        assert_eq!(parse_threads(" 8 "), Ok(Some(8)));
        assert_eq!(parse_threads(""), Ok(None));
        assert!(parse_threads("0").is_err());
        assert!(parse_threads("-1").is_err());
        assert!(parse_threads("eight").is_err());
    }

    #[test]
    fn the_cpu_is_the_row_s_own_backend_else_the_class_s() {
        assert!(on_cpu("cpu", Some("cuda")));
        assert!(on_cpu("", Some("cpu")));
        assert!(
            !on_cpu("", Some("cpu ")),
            "matched exactly, as audio.cpp does"
        );
        assert!(!on_cpu("", Some("cuda")));
        assert!(!on_cpu("", None));
    }

    #[test]
    fn a_cpu_row_inherits_the_class_args_without_the_gpu() {
        let owner = Owner::new();
        owner.with(|| {
            let s = settings("cuda", 4, 16, "topology");
            let p = RowPlacement {
                backend: RwSignal::new("cpu".into()),
                threads: RwSignal::new(String::new()),
            };
            assert_eq!(
                inherited_args(p, &s),
                "--security-opt\nlabel=disable\n-e\nNVIDIA_VISIBLE_DEVICES=void"
            );
            assert!(own_args_pass_gpu(
                p,
                "--device\nnvidia.com/gpu=all\n",
                Some(&s.audio)
            ));
            assert!(!own_args_pass_gpu(
                p,
                "--security-opt\nlabel=disable",
                Some(&s.audio)
            ));
            p.backend.set(String::new());
            assert!(inherited_args(p, &s).starts_with("--device\nnvidia.com/gpu=all"));
            let (backend, threads, clear) = p.body().unwrap();
            assert_eq!((backend, threads), (Value::Null, Value::Null));
            assert_eq!(clear, ["backend", "threads"]);
            p.backend.set("cpu".into());
            p.threads.set("8".into());
            let (backend, threads, clear) = p.body().unwrap();
            assert_eq!((backend, threads), (json!("cpu"), json!(8)));
            assert!(clear.is_empty());
        });
    }
}
