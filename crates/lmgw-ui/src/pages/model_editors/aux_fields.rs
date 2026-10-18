//! The aux editor's row fields beside the kind (decision-models design §3.3,
//! §3.9): the batch sizes, the projector, the CPU switch and its thread
//! count. What a draft saves, what it would run with, and the name clients
//! call it by.
//!
//! The thread count a blank field runs with comes from the gateway's own rule
//! ([`lmgw_api_types::aux_engine::threads_in_effect`]), read on this
//! machine's CPUs as `GET /api/settings-full` reports them, so the
//! placeholder says what the container would start with.

use leptos::prelude::*;
use lmgw_api_types::audio_engine::{cpu_run_args, passes_gpu};
use lmgw_api_types::aux_engine::{threads_in_effect, AuxThreadsSource};
use lmgw_api_types::{public_names, AuxModel, SettingsFull};
use serde_json::{json, Map, Value};

use crate::widgets::Select;

/// The five fields' drafts.
#[derive(Clone, Copy)]
pub(super) struct AuxFields {
    pub ubatch_size: RwSignal<String>,
    pub batch_size: RwSignal<String>,
    /// Relative to the aux models dir; blank is none.
    pub mmproj_path: RwSignal<String>,
    /// `""` runs the row on the GPU, `"cpu"` on the CPU.
    pub backend: RwSignal<String>,
    pub threads: RwSignal<String>,
}

impl AuxFields {
    pub fn new(m: &AuxModel) -> Self {
        let count = |n: Option<i64>| RwSignal::new(n.map(|n| n.to_string()).unwrap_or_default());
        Self {
            ubatch_size: count(m.ubatch_size),
            batch_size: count(m.batch_size),
            mmproj_path: RwSignal::new(m.mmproj_path.clone().unwrap_or_default()),
            backend: RwSignal::new(m.backend.clone().unwrap_or_default()),
            threads: count(m.threads),
        }
    }

    /// What the five fields hold now.
    pub fn draft(&self) -> FieldsDraft {
        FieldsDraft {
            ubatch_size: self.ubatch_size.get_untracked(),
            batch_size: self.batch_size.get_untracked(),
            mmproj_path: self.mmproj_path.get_untracked(),
            backend: self.backend.get_untracked(),
            threads: self.threads.get_untracked(),
        }
    }

    /// The row runs on the CPU as drafted.
    pub fn on_cpu(&self) -> bool {
        self.backend.with(|b| b.trim() == "cpu")
    }
}

/// The five fields as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct FieldsDraft {
    pub ubatch_size: String,
    pub batch_size: String,
    pub mmproj_path: String,
    pub backend: String,
    pub threads: String,
}

impl FieldsDraft {
    /// The patch's values for the fields that are set, and the names of the
    /// blank ones for `clear` (llama-server's default, no projector, the
    /// GPU, the thread count of [`threads_placeholder`]).
    pub fn body(&self) -> Result<(Map<String, Value>, Vec<&'static str>), String> {
        let mut set = Map::new();
        let mut clear = Vec::new();
        let counts = [
            ("ubatch_size", "ubatch size", &self.ubatch_size),
            ("batch_size", "batch size", &self.batch_size),
            ("threads", "threads", &self.threads),
        ];
        for (key, label, raw) in counts {
            match parse_count(label, raw)? {
                Some(n) => {
                    set.insert(key.into(), json!(n));
                }
                None => clear.push(key),
            }
        }
        for (key, raw) in [
            ("mmproj_path", &self.mmproj_path),
            ("backend", &self.backend),
        ] {
            match raw.trim() {
                "" => clear.push(key),
                v => {
                    set.insert(key.into(), json!(v));
                }
            }
        }
        Ok((set, clear))
    }
}

/// A count as typed: blank takes the default, otherwise a whole number of 1
/// or more (the columns' CHECKs).
fn parse_count(label: &str, raw: &str) -> Result<Option<i64>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!(
            "{label}: '{raw}' is not a count — a whole number of 1 or more, or blank for the \
             default"
        )),
    }
}

/// One argument per line, as the args fields take them.
pub(super) fn lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// What a blank thread count runs with and where it comes from, by the
/// gateway's rule: a count the extra args name, this machine's cores on the
/// CPU, llama-server's own on the GPU.
fn threads_placeholder(on_cpu: bool, args: &[String], s: Option<&SettingsFull>) -> String {
    let Some(s) = s else {
        return String::new();
    };
    match threads_in_effect(None, on_cpu, args, &s.host_cpu) {
        (Some(n), source) => format!("{n} · {}", source.label()),
        (None, AuxThreadsSource::Args) => "from the extra args (not a number)".into(),
        (None, source) => source.label().into(),
    }
}

/// The run args a blank override inherits, as the placeholder shows them: a
/// row on the CPU runs without the class's GPU passthrough.
pub(super) fn inherited_run_args(on_cpu: bool, s: &SettingsFull) -> String {
    let class = &s.aux_router.extra_run_args;
    let args = if on_cpu {
        cpu_run_args(class)
    } else {
        class.clone()
    };
    args.join("\n")
}

/// The name clients call the row by, as the gateway names it
/// ([`lmgw_api_types::public_names`]): the decision prefix for a decision
/// row, the aux class's public prefix for the others.
pub(super) fn public_name(kind: &str, model_id: &str, s: &SettingsFull) -> String {
    let a = &s.aux_router;
    public_names::prefixed(
        public_names::aux_prefix(kind, &a.public_prefix, &a.decision_prefix),
        model_id,
    )
}

/// The Engine card's fields: where the row runs, its thread count and the
/// two batch sizes. `args` is the extra llama-server flags' draft, whose
/// `--threads` the blank count yields to.
#[component]
pub(super) fn EngineFields(
    f: AuxFields,
    args: RwSignal<String>,
    #[prop(into)] settings: Signal<Option<SettingsFull>>,
) -> impl IntoView {
    let runs_on = Signal::derive(|| {
        vec![
            (String::new(), "GPU".to_string()),
            ("cpu".to_string(), "CPU".to_string()),
        ]
    });
    let placeholder = move || {
        let a = args.with(|a| lines(a));
        settings.with(|s| threads_placeholder(f.on_cpu(), &a, s.as_ref()))
    };
    view! {
        <div class="field-grid">
            <div class="field">
                <label>"Runs on"</label>
                <Select value=f.backend options=runs_on/>
            </div>
            <div class="field">
                <label>"Threads" <span class="field-unit">"blank: as shown"</span></label>
                <input class="input mono"
                    placeholder=placeholder
                    prop:value=move || f.threads.get()
                    on:input=move |ev| f.threads.set(event_target_value(&ev))/>
            </div>
            <div class="field">
                <label>"Ubatch size" <span class="field-unit">"blank = default"</span></label>
                <input class="input mono"
                    prop:value=move || f.ubatch_size.get()
                    on:input=move |ev| f.ubatch_size.set(event_target_value(&ev))/>
            </div>
            <div class="field">
                <label>"Batch size" <span class="field-unit">"blank = default"</span></label>
                <input class="input mono"
                    prop:value=move || f.batch_size.get()
                    on:input=move |ev| f.batch_size.set(event_target_value(&ev))/>
            </div>
        </div>
        <p class="dim mini-note">
            "On the CPU the row takes no GPU memory and keeps serving while the GPU hold is on; "
            "its inherited run args lose the GPU passthrough. A blank thread count yields to a "
            "--threads in the extra flags, else runs this machine's cores there, as shown. In embedding "
            "mode llama-server lowers the batch to the ubatch, so raise both together: an "
            "encoder it cannot split (any but a last-token embedder or a causal reranker) "
            "reads each input in one ubatch. On the GPU the ledger charges the compute buffer "
            "they grow."
        </p>
    }
}

/// The hold-fallback note for a row on the CPU, which the hold never stops.
#[component]
pub(super) fn CpuHoldNote(f: AuxFields) -> impl IntoView {
    move || {
        f.on_cpu().then(|| {
            view! {
                <p class="dim mini-note">
                    "Not used while this row runs on the CPU: the GPU hold does not stop a CPU "
                    "row, so it keeps answering itself."
                </p>
            }
        })
    }
}

/// The warning under the run-args override of a CPU row whose own args
/// still pass a GPU: they are used as written.
#[component]
pub(super) fn CpuGpuArgsNote(f: AuxFields, own: RwSignal<String>) -> impl IntoView {
    move || {
        (f.on_cpu() && own.with(|o| passes_gpu(&lines(o)))).then(|| {
            view! {
                <div class="notice warn">
                    "This row runs on the CPU, but these args still pass the GPU to its "
                    "container. They are used as written, so llama-server can still open a CUDA "
                    "context on the card: remove those flags, or clear the override to inherit "
                    "the class args without them."
                </div>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::HostCpu;

    fn settings(cores: u32, source: &str) -> SettingsFull {
        let mut s = SettingsFull {
            host_cpu: HostCpu {
                physical_cores: cores,
                logical_cpus: cores * 2,
                source: source.into(),
            },
            ..Default::default()
        };
        s.aux_router.extra_run_args = [
            "--device",
            "nvidia.com/gpu=all",
            "--security-opt",
            "label=disable",
        ]
        .map(String::from)
        .to_vec();
        s.aux_router.public_prefix = "embed".into();
        s.aux_router.decision_prefix = "decide/".into();
        s
    }

    fn row() -> AuxModel {
        AuxModel {
            id: 1,
            model_id: "laya".into(),
            gguf_path: "laya.gguf".into(),
            kind: "decision".into(),
            pooling: None,
            ctx_size: Some(8192),
            ubatch_size: Some(8192),
            batch_size: None,
            mmproj_path: None,
            backend: Some("cpu".into()),
            threads: None,
            args: Vec::new(),
            idle_seconds: 300,
            enabled: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: "inherit".into(),
            hold_fallback: None,
        }
    }

    #[test]
    fn a_blank_thread_count_says_what_the_container_starts_with() {
        let s = settings(16, "topology");
        assert_eq!(
            threads_placeholder(true, &[], Some(&s)),
            "16 · physical cores"
        );
        assert_eq!(
            threads_placeholder(false, &[], Some(&s)),
            "llama-server's default"
        );
        let args = lines("--threads\n4\n");
        assert_eq!(
            threads_placeholder(true, &args, Some(&s)),
            "4 · from the extra args"
        );
        assert_eq!(
            threads_placeholder(false, &lines("-t=x"), Some(&s)),
            "from the extra args (not a number)"
        );
        let odd = settings(32, "logical");
        assert!(threads_placeholder(true, &[], Some(&odd)).starts_with("32 · logical CPUs"));
        assert_eq!(threads_placeholder(true, &[], None), "");
    }

    #[test]
    fn a_count_is_whole_and_positive_or_blank() {
        assert_eq!(parse_count("ubatch size", " 8192 "), Ok(Some(8192)));
        assert_eq!(parse_count("ubatch size", ""), Ok(None));
        assert!(parse_count("ubatch size", "0").is_err());
        assert!(parse_count("ubatch size", "-1").is_err());
        assert!(parse_count("batch size", "lots")
            .unwrap_err()
            .starts_with("batch size: 'lots'"));
    }

    #[test]
    fn the_body_sets_what_is_filled_and_clears_what_is_blank() {
        let owner = Owner::new();
        owner.with(|| {
            let f = AuxFields::new(&row());
            let (set, clear) = f.draft().body().unwrap();
            assert_eq!(
                Value::Object(set),
                json!({"ubatch_size": 8192, "backend": "cpu"})
            );
            assert_eq!(clear, ["batch_size", "threads", "mmproj_path"]);
            f.backend.set(String::new());
            f.threads.set("8".into());
            f.mmproj_path.set(" mm/proj.gguf ".into());
            let (set, clear) = f.draft().body().unwrap();
            assert_eq!(set["threads"], json!(8));
            assert_eq!(set["mmproj_path"], json!("mm/proj.gguf"));
            assert_eq!(clear, ["batch_size", "backend"]);
            f.batch_size.set("0".into());
            assert!(f.draft().body().is_err());
        });
    }

    #[test]
    fn a_cpu_row_inherits_the_class_args_without_the_gpu() {
        let s = settings(16, "topology");
        assert_eq!(
            inherited_run_args(true, &s),
            "--security-opt\nlabel=disable\n-e\nNVIDIA_VISIBLE_DEVICES=void"
        );
        assert!(inherited_run_args(false, &s).starts_with("--device\nnvidia.com/gpu=all"));
    }

    #[test]
    fn the_public_name_takes_the_kind_s_prefix() {
        let mut s = settings(16, "topology");
        assert_eq!(public_name("decision", "laya", &s), "decide/laya");
        assert_eq!(public_name("embed", "bge", &s), "embed/bge");
        assert_eq!(public_name("rerank", "rr", &s), "embed/rr");
        s.aux_router.decision_prefix = String::new();
        assert_eq!(public_name("decision", "laya", &s), "laya");
    }
}
