//! What an aux row shows on the Models page (decision-models design §3.9):
//! where it runs in its file cell, as audio rows say it, and what its load
//! test sent and found, by the probe the gateway sent
//! (`ModelTestPassed.probe`): an embedding's length, how many documents a
//! reranker scored, or a decision row's answer with the input tokens its
//! question took (§3.7).

use lmgw_api_types::{AuxModelView, DecideAnswer, ModelTestPassed};

/// The file cell: the GGUF's name, and on the CPU the thread count its
/// container starts with — a CPU row holds nothing on the GPU, so this is
/// what there is to say about its cost.
pub(super) fn file(v: &AuxModelView) -> String {
    let name = super::file_name(&v.model.gguf_path);
    match (on_cpu(v), v.threads_in_effect) {
        (true, Some(n)) => format!("{name} · CPU · {n} threads"),
        (true, None) => format!("{name} · CPU"),
        (false, _) => name,
    }
}

/// The file cell's tooltip: the paths, where the row is placed and its
/// thread count with where that comes from. A row without `backend` is
/// placed on the GPU for the hold and the ledger even when its args keep
/// `--n-gpu-layers 0` (a row the boot hoist left on a GPU image), so the
/// tooltip names the flag rather than claiming where it computes.
pub(super) fn file_title(v: &AuxModelView) -> String {
    let mut out = v.model.gguf_path.clone();
    if let Some(p) = &v.model.mmproj_path {
        out.push_str(&format!("\nprojector: {p}"));
    }
    let threads = match v.threads_in_effect {
        Some(n) => format!("{n} ({})", v.threads_source.label()),
        None => v.threads_source.label().to_string(),
    };
    let place = match (on_cpu(v), args_offload_nothing(&v.model.args)) {
        (true, _) => "runs on the CPU",
        (false, true) => "placed on the GPU; its extra args carry --n-gpu-layers 0",
        (false, false) => "placed on the GPU",
    };
    out.push_str(&format!("\n{place} · threads: {threads}"));
    out
}

fn on_cpu(v: &AuxModelView) -> bool {
    v.model.backend.as_deref() == Some("cpu")
}

/// The freeform args offload no layers (`--n-gpu-layers 0`, `-ngl 0`,
/// `--gpu-layers 0`, or the `=0` forms).
fn args_offload_nothing(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let flag = a.trim_start_matches('-');
        if !a.starts_with('-') {
            continue;
        }
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (flag, None),
        };
        if matches!(key, "n-gpu-layers" | "ngl" | "gpu-layers") {
            let value = inline.or_else(|| it.next().map(String::as_str));
            if value.map(str::trim) == Some("0") {
                return true;
            }
        }
    }
    false
}

/// What the probe on an aux row does, for the Test button's tooltip.
pub(super) fn probe_of(kind: &str) -> &'static str {
    match kind {
        "decision" => "asks one System One choice question on /v1/systemone and shows the answer",
        "rerank" => "reranks two probe documents",
        _ => "embeds a probe input",
    }
}

/// The toast of an aux row that loaded and answered its probe.
pub(super) fn passed(model_id: &str, p: &ModelTestPassed) -> String {
    let ms = format!(" in {} ms", p.latency_ms);
    let tokens = p
        .input_tokens
        .map(|n| format!(", {n} input tokens"))
        .unwrap_or_default();
    match (p.probe.as_str(), p.answer.as_deref()) {
        ("decide", Some(a)) => {
            format!("{model_id}: loads and decides{ms}{tokens} — {}", answer(a))
        }
        ("decide", None) => format!("{model_id}: loads and decides{ms}{tokens}"),
        ("rerank", _) => {
            let scored = p
                .scored
                .map(|n| format!(" ({n} documents scored)"))
                .unwrap_or_default();
            format!("{model_id}: loads and reranks{scored}{ms}")
        }
        _ => {
            let dims = p
                .dimensions
                .map(|d| format!(" ({d} dimensions)"))
                .unwrap_or_default();
            format!("{model_id}: loads and embeds{dims}{ms}")
        }
    }
}

/// An answer in one line: the choice with every option's probability, the
/// most probable first.
fn answer(a: &DecideAnswer) -> String {
    let mut options: Vec<(&str, f64)> = a
        .probabilities
        .iter()
        .map(|o| (o.option.as_str(), o.probability))
        .collect();
    options.sort_by(|a, b| b.1.total_cmp(&a.1));
    format!(
        "chose {} ({})",
        a.choice,
        options
            .iter()
            .map(|(option, p)| format!("{option} {p:.3}"))
            .collect::<Vec<_>>()
            .join(" · ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pass(probe: &str, extra: serde_json::Value) -> ModelTestPassed {
        let mut v = json!({
            "ok": true, "loaded": true, "model_id": "m", "class": "aux",
            "probe": probe, "latency_ms": 412,
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    fn view(backend: Option<&str>, threads: Option<i64>, source: &str) -> AuxModelView {
        serde_json::from_value(json!({
            "public_name": "decide/laya",
            "model": {
                "id": 1, "model_id": "laya", "gguf_path": "TypeSafe/Laya-Q8_0.gguf",
                "kind": "decision", "backend": backend, "mmproj_path": null,
                "idle_seconds": 300, "enabled": true,
            },
            "threads_in_effect": threads,
            "threads_source": source,
        }))
        .unwrap()
    }

    #[test]
    fn a_cpu_row_says_so_with_its_thread_count_as_audio_rows_do() {
        let cpu = view(Some("cpu"), Some(16), "cores");
        assert_eq!(file(&cpu), "Laya-Q8_0.gguf · CPU · 16 threads");
        assert_eq!(
            file_title(&cpu),
            "TypeSafe/Laya-Q8_0.gguf\nruns on the CPU · threads: 16 (physical cores)"
        );
        assert_eq!(
            file(&view(Some("cpu"), None, "args")),
            "Laya-Q8_0.gguf · CPU"
        );
        let mut gpu = view(None, None, "llama_server");
        assert_eq!(file(&gpu), "Laya-Q8_0.gguf");
        assert!(file_title(&gpu).ends_with("\nplaced on the GPU · threads: llama-server's default"));
        // A row the boot hoist kept on a GPU image: GPU-placed, -ngl 0 in args.
        for args in [
            &["--n-gpu-layers", "0"][..],
            &["-ngl", "0"],
            &["--gpu-layers=0"],
        ] {
            gpu.model.args = args.iter().map(|s| s.to_string()).collect();
            assert!(
                file_title(&gpu)
                    .contains("placed on the GPU; its extra args carry --n-gpu-layers 0"),
                "{args:?}"
            );
        }
        gpu.model.args = vec!["-ngl".into(), "99".into()];
        assert!(file_title(&gpu).contains("\nplaced on the GPU · "));
    }

    /// The toast puts the most probable option first, whatever order the
    /// model answered them in.
    #[test]
    fn a_decision_pass_shows_the_answer_and_its_input_tokens() {
        let p: ModelTestPassed = serde_json::from_str(
            r#"{"ok": true, "loaded": true, "model_id": "laya", "class": "aux",
                "probe": "decide", "latency_ms": 412, "input_tokens": 31,
                "answer": {"choice": "software",
                           "probabilities": [{"option": "cooking", "probability": 0.0111},
                                             {"option": "software", "probability": 0.98213},
                                             {"option": "sports", "probability": 0.00677}],
                           "confidence": 0.9}}"#,
        )
        .unwrap();
        assert_eq!(
            passed("laya", &p),
            "laya: loads and decides in 412 ms, 31 input tokens — chose software (software \
             0.982 · cooking 0.011 · sports 0.007)"
        );
    }

    #[test]
    fn the_other_probes_say_what_they_measured() {
        assert_eq!(
            passed("bge", &pass("embed", json!({"dimensions": 1024}))),
            "bge: loads and embeds (1024 dimensions) in 412 ms"
        );
        assert_eq!(
            passed("rr", &pass("rerank", json!({"scored": 2}))),
            "rr: loads and reranks (2 documents scored) in 412 ms"
        );
        assert_eq!(
            passed("laya", &pass("decide", json!({"input_tokens": 41}))),
            "laya: loads and decides in 412 ms, 41 input tokens",
            "a decide pass without its answer still says it decided"
        );
    }

    #[test]
    fn the_tooltip_names_each_kind_s_probe() {
        assert!(probe_of("decision").contains("/v1/systemone"));
        assert_eq!(probe_of("rerank"), "reranks two probe documents");
        assert_eq!(probe_of("embed"), "embeds a probe input");
    }
}
