//! What a run was (benchmark design §6): the model file, the build, the GPU,
//! the server as it answered and the suite, the effective settings — each
//! override marked, with what the row says now beside it — and the command
//! line the bench container was started with.

use leptos::prelude::*;
use lmgw_api_types::bench::CLOCK_EVENT_REASONS;
use lmgw_api_types::bench_ops::{BenchOverrides, BenchRun};
use lmgw_api_types::LocalModel;
use serde_json::Value;

use super::{fmt_ms, phase_label};
use crate::fmt::{grouped, human_bytes};
use crate::pages::backends::{local_ts, sha7, short_repo};
use crate::widgets::CopyBtn;

/// A definition list of `label → value` lines; a `None` value is left out.
fn kv(rows: Vec<(&'static str, Option<AnyView>)>) -> AnyView {
    let lines = rows
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| view! { <dt>{k}</dt><dd>{v}</dd> }))
        .collect_view();
    view! { <dl class="bn-kv">{lines}</dl> }.into_any()
}

fn text(s: impl Into<String>) -> Option<AnyView> {
    let s = s.into();
    Some(view! { <span>{s}</span> }.into_any())
}

fn mono(s: impl Into<String>) -> Option<AnyView> {
    let s = s.into();
    Some(view! { <span class="mono-sm">{s}</span> }.into_any())
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Option<AnyView>) -> Option<AnyView> {
    v.and_then(f)
}

/// The file's name, with the whole path copyable and in the tooltip.
fn path(p: &str) -> Option<AnyView> {
    let name = p.rsplit('/').next().unwrap_or(p).to_string();
    let full = p.to_string();
    let tip = full.clone();
    Some(
        view! {
            <span class="mono-sm bn-path" title=tip>
                <CopyBtn text=full title="Copy the path"/>
                {name}
            </span>
        }
        .into_any(),
    )
}

#[component]
pub fn IdentityCards(run: BenchRun, row: Option<LocalModel>) -> impl IntoView {
    let m = run.model.clone();
    let b = run.build.clone();
    let g = run.gpu.clone();
    let server = run.results.server.clone();
    let energy = run.results.energy.clone();
    let p = run.params.clone();

    let model = kv(vec![
        ("row", mono(m.model_id.clone())),
        ("weights", path(&m.gguf_path)),
        ("size", text(human_bytes(m.gguf_size))),
        (
            "modified",
            opt(m.gguf_mtime.clone(), |t| text(local_ts(&t))),
        ),
        ("quant", opt(m.quant.clone(), mono)),
        (
            "rung",
            text(if m.rung == 0 {
                "base".to_string()
            } else {
                format!("rung {}", m.rung)
            }),
        ),
    ]);
    let build = kv(vec![
        (
            "image",
            Some({
                let r = b.image_ref.clone();
                let (tip, copy) = (r.clone(), r.clone());
                view! {
                    <span class="mono-sm bn-path" title=tip>
                        <CopyBtn text=copy title="Copy the image reference"/>
                        {r}
                    </span>
                }
                .into_any()
            }),
        ),
        ("engine", opt(b.engine_slug.clone(), mono)),
        (
            "source",
            opt(b.repo.clone(), |r| {
                let at = b
                    .git_ref
                    .clone()
                    .map(|g| format!(" @ {g}"))
                    .unwrap_or_default();
                mono(format!("{}{at}", short_repo(&r)))
            }),
        ),
        (
            "commit",
            opt(b.commit.clone(), |c| {
                Some(view! { <span class="mono-sm" title=c.clone()>{sha7(&c)}</span> }.into_any())
            }),
        ),
        ("version", opt(b.version.clone(), mono)),
        ("build_info", opt(b.build_info.clone(), mono)),
        (
            "image id",
            opt(b.image_id.clone(), |i| {
                let short: String = i.trim_start_matches("sha256:").chars().take(12).collect();
                Some(view! { <span class="mono-sm" title=i>{short}</span> }.into_any())
            }),
        ),
    ]);
    let events: Vec<&str> = CLOCK_EVENT_REASONS
        .iter()
        .filter(|(bit, _, _)| g.clock_events & bit != 0)
        .map(|(_, n, _)| *n)
        .collect();
    let gpu = kv(vec![
        ("card", opt(g.name.clone(), text)),
        ("driver", opt(g.driver.clone(), mono)),
        ("VRAM", opt(g.vram_total_bytes, |v| text(human_bytes(v)))),
        (
            "power limit",
            opt(g.power_limit_w, |w| text(format!("{w:.0} W"))),
        ),
        (
            "temperature",
            match (g.temp_start_c, g.temp_end_c, g.temp_max_c) {
                (Some(a), Some(z), Some(m)) => text(format!("{a} → {z} °C, peak {m} °C")),
                (Some(a), _, Some(m)) => text(format!("from {a} °C, peak {m} °C")),
                _ => None,
            },
        ),
        (
            "held back",
            Some(if g.throttled {
                view! { <span class="bn-warn-text">"yes — these are throttled numbers"</span> }
                    .into_any()
            } else {
                view! { <span>"no"</span> }.into_any()
            }),
        ),
        (
            "clock events",
            (!events.is_empty())
                .then(|| mono(events.join(", ")))
                .flatten(),
        ),
        (
            "telemetry",
            (!g.telemetry.is_empty())
                .then(|| text(g.telemetry.clone()))
                .flatten(),
        ),
    ]);
    let phases: Vec<&str> = p.phases.iter().map(|ph| phase_label(*ph)).collect();
    let suite = kv(vec![
        (
            "suite",
            text(format!(
                "v{} · {} repetitions",
                p.suite_version, p.repetitions
            )),
        ),
        ("phases", text(phases.join(", "))),
        (
            "generated",
            text(format!("{} tokens per decode request", p.generate_tokens)),
        ),
        (
            "sampling",
            text(format!(
                "temperature {} · top_p {} · top_k {} · seed {}",
                p.sampling.temperature, p.sampling.top_p, p.sampling.top_k, p.sampling.seed
            )),
        ),
        (
            "slots",
            opt(server.as_ref(), |s| {
                text(format!(
                    "{} × {} tokens (from {})",
                    s.n_slots,
                    grouped(s.per_slot_ctx),
                    s.per_slot_ctx_source
                ))
            }),
        ),
        (
            "corpus",
            opt(run.results.corpus_tokens, |t| {
                text(format!("{} tokens", grouped(t)))
            }),
        ),
        (
            "speculative",
            opt(server.as_ref().and_then(|s| s.speculative), |v| {
                text(if v { "yes" } else { "no" })
            }),
        ),
        (
            "idle power",
            opt(energy.idle_power_w, |w| {
                text(format!(
                    "{w:.0} W, baseline VRAM {}",
                    energy
                        .baseline_vram_bytes
                        .map(human_bytes)
                        .unwrap_or_else(|| "—".into())
                ))
            }),
        ),
        (
            "peak",
            match (energy.peak_power_w, energy.peak_vram_bytes) {
                (Some(w), Some(v)) => text(format!("{w:.0} W · {}", human_bytes(v))),
                (Some(w), None) => text(format!("{w:.0} W")),
                _ => None,
            },
        ),
        (
            "energy",
            match (energy.total_joules, energy.unavailable.clone()) {
                (Some(j), _) => text(format!(
                    "{:.1} kJ over the run ({})",
                    j / 1000.0,
                    match energy.source {
                        Some(lmgw_api_types::bench::EnergySource::Integrated) => "power integrated",
                        _ => "the driver's energy counter",
                    }
                )),
                (None, Some(why)) => text(why),
                _ => None,
            },
        ),
        (
            "load",
            opt(run.results.load.as_ref(), |l| {
                text(format!("{} to healthy", fmt_ms(l.ms as f64)))
            }),
        ),
    ]);

    view! {
        <div class="auto-grid bn-ident">
            <div class="card edit-section">
                <h3>"Model"</h3>
                {model}
            </div>
            <div class="card edit-section">
                <h3>"Build"</h3>
                {build}
            </div>
            <div class="card edit-section">
                <h3>"GPU"</h3>
                {gpu}
            </div>
            <div class="card edit-section">
                <h3>"Server and suite"</h3>
                {suite}
            </div>
        </div>
        <SettingsCard run=run.clone() row=row/>
        <div class="card edit-section bn-cmd">
            <div class="cmd-head">
                <h3>"Command line"</h3>
                <span class="dim">"as the bench container was started (port and name are the run's own)"</span>
                <CopyBtn text=run.command_line.clone() title="Copy the command line"/>
            </div>
            <pre class="preset cmd-lines">
                {if run.command_line.is_empty() {
                    "not recorded yet: the run stores it once its container has a port".to_string()
                } else {
                    run.command_line.clone()
                }}
            </pre>
        </div>
    }
}

/// The llama-server settings a reader compares runs by, in this order.
const KEYS: &[(&str, &str)] = &[
    ("ctx_size", "context"),
    ("parallel", "slots"),
    ("batch_size", "batch"),
    ("ubatch_size", "ubatch"),
    ("cache_type_k", "cache K"),
    ("cache_type_v", "cache V"),
    ("flash_attn", "flash attention"),
    ("kv_unified", "unified KV"),
    ("n_gpu_layers", "GPU layers"),
    ("threads", "threads"),
    ("fit", "fit"),
    ("spec_type", "speculative"),
    ("draft_gguf_path", "drafter"),
    ("mmproj_path", "projector"),
];

/// Whether an override set `key` (§3.5).
fn overridden(o: &BenchOverrides, key: &str) -> bool {
    match key {
        "ctx_size" => o.ctx_size.is_some(),
        "parallel" => o.parallel.is_some(),
        "batch_size" => o.batch_size.is_some(),
        "ubatch_size" => o.ubatch_size.is_some(),
        "cache_type_k" => o.cache_type_k.is_some(),
        "cache_type_v" => o.cache_type_v.is_some(),
        "flash_attn" => o.flash_attn.is_some(),
        "kv_unified" => o.kv_unified.is_some(),
        "n_gpu_layers" => o.n_gpu_layers.is_some(),
        "spec_type" | "draft_gguf_path" => o.no_draft,
        _ => false,
    }
}

pub fn show_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "server default".into(),
        Some(Value::Bool(b)) => if *b { "on" } else { "off" }.into(),
        Some(Value::Number(n)) => n.as_u64().map(grouped).unwrap_or_else(|| n.to_string()),
        Some(Value::String(s)) if s.contains('/') => s.rsplit('/').next().unwrap_or(s).to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

#[component]
fn SettingsCard(run: BenchRun, row: Option<LocalModel>) -> impl IntoView {
    let s = run.settings.clone();
    let row_params = row
        .as_ref()
        .and_then(|r| serde_json::to_value(&r.params).ok());
    let lines = KEYS
        .iter()
        .map(|(key, label)| {
            let v = s.params.get(*key);
            let over = overridden(&s.overrides, key);
            let now = row_params.as_ref().map(|p| show_value(p.get(*key)));
            let effective = show_value(v);
            let full = v.and_then(Value::as_str).map(str::to_string).unwrap_or_default();
            let changed_since = !over && now.as_ref().is_some_and(|n| *n != effective);
            view! {
                <tr class:bn-over=over>
                    <td class="dim">{*label}</td>
                    <td class="mono-sm" title=full>{effective}</td>
                    <td class="wrap">
                        {over.then(|| view! { <span class="type-badge bn-over-badge">"override"</span> })}
                        {(over || changed_since)
                            .then(|| {
                                now.clone()
                                    .map(|n| view! { <span class="dim bn-rownow">{format!("row now: {n}")}</span> })
                            })}
                    </td>
                </tr>
            }
        })
        .collect_view();
    let args = (!s.args.is_empty()).then(|| s.args.join(" "));
    let run_args = (!s.extra_run_args.is_empty()).then(|| s.extra_run_args.join(" "));
    let hash: String = run.settings_hash.chars().take(12).collect();
    view! {
        <div class="card edit-section bn-settings">
            <div class="cmd-head">
                <h3>"Settings"</h3>
                <span class="dim" title=run.settings_hash.clone()>
                    {format!("settings hash {hash} — runs compare only with the same one")}
                </span>
            </div>
            <table class="data bk-sub bn-set-t">
                <tbody>{lines}</tbody>
            </table>
            {args.map(|a| view! { <div class="bn-args"><span class="dim">"freeform flags "</span><code>{a}</code></div> })}
            {run_args.map(|a| view! { <div class="bn-args"><span class="dim">"podman run "</span><code>{a}</code></div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_read_as_settings() {
        assert_eq!(show_value(None), "server default");
        assert_eq!(show_value(Some(&json!(null))), "server default");
        assert_eq!(show_value(Some(&json!(true))), "on");
        assert_eq!(show_value(Some(&json!(262144))), "262\u{202F}144");
        assert_eq!(show_value(Some(&json!("q8_0"))), "q8_0");
        assert_eq!(show_value(Some(&json!("a/b/mtp.gguf"))), "mtp.gguf");
    }

    #[test]
    fn an_override_marks_its_key_and_no_draft_marks_the_drafter() {
        let o = BenchOverrides {
            ctx_size: Some(8192),
            no_draft: true,
            ..Default::default()
        };
        assert!(overridden(&o, "ctx_size"));
        assert!(overridden(&o, "draft_gguf_path"));
        assert!(!overridden(&o, "parallel"));
    }
}
