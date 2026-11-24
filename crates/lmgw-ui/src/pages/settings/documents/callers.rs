//! Settings › Documents' "Who calls the docling route" (document-conversion
//! design §7.8, §8): per key, how often it called the docling-serve routes
//! and, in plain words, what it asked for that lmgw handles differently
//! (picture descriptions, the OCR engine easyocr, web addresses). The expert
//! view adds the routes and every option with its value, outcome and count.
//! Read from `doc_adapter_usage` each time the category is shown.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::documents::adapter_usage::{
    AdapterKeyRow, AdapterUsage, OptionOutcome, OptionSeen,
};
use serde_json::json;

/// What an option asks for, in plain words.
pub(super) fn asked(o: &OptionSeen) -> String {
    let v = &o.value;
    match o.name.as_str() {
        n if n.starts_with("picture_description") || n == "do_picture_description" => {
            "picture descriptions".into()
        }
        "ocr_engine" | "ocr_preset" => format!("the OCR engine \"{v}\""),
        "force_ocr" => "text recognition on every page".into(),
        "ocr_lang" => format!("OCR languages {v}"),
        "to_formats" => format!("the outputs {v}"),
        "image_export_mode" | "include_images" | "images_scale" => "pictures in the output".into(),
        "include_page_images" => "page images".into(),
        "pipeline" => format!("the {v} pipeline"),
        n if n.starts_with("vlm_pipeline") => "a model reading the pages".into(),
        n if n.contains("picture_classification") => "picture classification".into(),
        n if n.contains("chart_extraction") => "chart extraction".into(),
        n if n.contains("formula")
            || n.contains("code_enrichment")
            || n.contains("code_formula") =>
        {
            "formulas and code read by a model".into()
        }
        n if n.starts_with("chunking") => "chunking".into(),
        "callbacks" => "callbacks when done".into(),
        "headers" => "connection headers on web addresses".into(),
        n if n.starts_with("table") || n.starts_with("layout") => {
            "docling's table and layout models".into()
        }
        "pdf_backend" => "a PDF reader of its choice".into(),
        n if n.contains("heading") => "heading levels".into(),
        other => format!("the option {other}"),
    }
}

fn outcome_said(o: &OptionSeen) -> &'static str {
    if o.detail.as_deref() == Some("model_not_chosen") {
        return "no model chosen";
    }
    match o.outcome {
        OptionOutcome::Mapped => "lmgw's own",
        OptionOutcome::NotYet => "not built yet",
        OptionOutcome::Refused => "refused",
        OptionOutcome::Honoured => "as docling does",
    }
}

/// The plain line: what the key asks for that lmgw handles differently.
pub(super) fn plain_line(k: &AdapterKeyRow) -> String {
    let mut seen: Vec<String> = Vec::new();
    for o in k
        .options
        .iter()
        .filter(|o| o.outcome != OptionOutcome::Honoured)
    {
        let s = format!("{} ({})", asked(o), outcome_said(o));
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    // One entry per host, its outcomes in the order seen.
    let mut by_host: Vec<(&str, Vec<&str>)> = Vec::new();
    for h in &k.hosts {
        let said = match h.outcome.as_str() {
            "fetched" => "downloaded",
            "failed" => "download failed",
            _ => "refused",
        };
        match by_host.iter_mut().find(|(n, _)| *n == h.host.as_str()) {
            Some((_, o)) if !o.contains(&said) => o.push(said),
            Some(_) => {}
            None => by_host.push((h.host.as_str(), vec![said])),
        }
    }
    let hosts: Vec<String> = by_host
        .into_iter()
        .map(|(h, o)| format!("{h} ({})", o.join(", ")))
        .collect();
    if !hosts.is_empty() {
        seen.push(format!("web addresses from {}", hosts.join(", ")));
    }
    if seen.is_empty() {
        "Nothing lmgw handles differently.".to_string()
    } else {
        format!("Asks for: {}.", seen.join(", "))
    }
}

fn expert_line(k: &AdapterKeyRow) -> String {
    let routes: Vec<String> = k
        .routes
        .iter()
        .map(|r| format!("{} ×{}", r.route, r.count))
        .collect();
    let options: Vec<String> = k
        .options
        .iter()
        .map(|o| match &o.detail {
            Some(d) => format!(
                "{}={} {}({d}) ×{}",
                o.name,
                o.value,
                o.outcome.as_str(),
                o.count
            ),
            None => format!("{}={} {} ×{}", o.name, o.value, o.outcome.as_str(), o.count),
        })
        .collect();
    let mut s = format!("{} · {} files", routes.join(", "), k.files);
    if !options.is_empty() {
        s.push_str(" · ");
        s.push_str(&options.join(", "));
    }
    s
}

fn requests_said(k: &AdapterKeyRow) -> String {
    let when = crate::pages::backends::parse_ts(&k.last_seen)
        .map(crate::fmt::rel_time)
        .unwrap_or_else(|| k.last_seen.clone());
    let n = if k.requests == 1 {
        "1 request".to_string()
    } else {
        format!("{} requests", k.requests)
    };
    format!("{n}, last {when}")
}

#[component]
pub(super) fn Callers(id: String, hidden: Signal<bool>, expert: RwSignal<bool>) -> impl IntoView {
    let usage: RwSignal<Option<Result<AdapterUsage, String>>> = RwSignal::new(None);
    Effect::new(move |_| {
        if hidden.get() {
            return;
        }
        spawn_local(async move {
            let r = crate::api::post::<AdapterUsage, _>("/api/op/doc_adapter_usage", &json!({}))
                .await
                .map_err(|e| e.to_string());
            usage.try_set(Some(r));
        });
    });
    let body = move || {
        match usage.get() {
        None => view! { <div class="prow notice-row">"Reading…"</div> }.into_any(),
        Some(Err(e)) => view! {
            <div class="prow notice-row warn">{format!("Reading who calls the docling route failed: {e}")}</div>
        }
        .into_any(),
        Some(Ok(u)) if u.keys.is_empty() => view! {
            <div class="prow notice-row">
                "No tool has called the docling route yet. Tools that use docling-serve can \
                 point at lmgw instead; what they ask for shows here."
            </div>
        }
        .into_any(),
        Some(Ok(u)) => u
            .keys
            .into_iter()
            .map(|k| {
                let name = if k.key.is_empty() {
                    "Without a key".to_string()
                } else {
                    k.key.clone()
                };
                let detail = expert_line(&k);
                let attr = name.clone();
                view! {
                    <div class="prow doc-row" data-caller=attr>
                        <div class="prow-text">
                            <span class="prow-title">{name}</span>
                            <span class="prow-sub">{plain_line(&k)}</span>
                        </div>
                        <span class="prow-value">{requests_said(&k)}</span>
                        <div class="prow-extra doc-expert" hidden=move || !expert.get()>{detail}</div>
                    </div>
                }
            })
            .collect_view()
            .into_any(),
    }
    };
    view! {
        <div class="doc-rows" id=id hidden=hidden>
            {body}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::adapter_usage::{HostSeen, RouteSeen};

    #[test]
    fn the_plain_line_names_what_lmgw_handles_differently() {
        let o = |name: &str, value: &str, outcome| OptionSeen {
            name: name.into(),
            value: value.into(),
            outcome,
            detail: None,
            count: 3,
        };
        let mut k = AdapterKeyRow {
            key_id: Some(1),
            key: "example-key-1".into(),
            last_seen: "2026-11-18T10:00:00Z".into(),
            requests: 41,
            files: 50,
            routes: vec![RouteSeen {
                route: "/v1/convert/file".into(),
                count: 41,
            }],
            options: vec![
                OptionSeen {
                    detail: Some("model_not_chosen".into()),
                    ..o("do_picture_description", "true", OptionOutcome::Mapped)
                },
                o("ocr_engine", "easyocr", OptionOutcome::Mapped),
                o("do_ocr", "true", OptionOutcome::Honoured),
            ],
            hosts: Vec::new(),
        };
        assert_eq!(
            plain_line(&k),
            "Asks for: picture descriptions (no model chosen), the OCR engine \"easyocr\" (lmgw's own)."
        );
        k.options.clear();
        k.options.push(o("do_ocr", "true", OptionOutcome::Honoured));
        assert_eq!(plain_line(&k), "Nothing lmgw handles differently.");
        k.hosts.push(HostSeen {
            host: "docs.example".into(),
            outcome: "refused".into(),
            count: 1,
        });
        assert_eq!(
            plain_line(&k),
            "Asks for: web addresses from docs.example (refused)."
        );
        for (host, outcome) in [("files.example", "fetched"), ("docs.example", "failed")] {
            k.hosts.push(HostSeen {
                host: host.into(),
                outcome: outcome.into(),
                count: 2,
            });
        }
        assert_eq!(
            plain_line(&k),
            "Asks for: web addresses from docs.example (refused, download failed), files.example \
             (downloaded)."
        );
        assert!(expert_line(&k).starts_with("/v1/convert/file ×41 · 50 files · do_ocr=true"));
    }
}
