//! A new set run's file (labs-store design §3.3): pasted into the box, or a
//! CSV or JSONL file dropped or picked, which the page reads whole and
//! posts as text; no size is guessed for it, the body has no limit. A read
//! file shows as its name and size instead of the box, so a large file is
//! not put into a textarea. The format follows the file's extension or the
//! text's first character until it is picked by hand.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::BatchFormat;
use wasm_bindgen_futures::JsFuture;

use super::page::Lab;
use crate::fmt::human_bytes;

/// A picked file's text, read whole.
async fn read_text(file: web_sys::File) -> Result<String, String> {
    let v = JsFuture::from(file.text())
        .await
        .map_err(|_| format!("{} could not be read", file.name()))?;
    v.as_string()
        .ok_or_else(|| format!("{} did not read as text", file.name()))
}

fn take(lab: Lab, file: Option<web_sys::File>) {
    let Some(file) = file else { return };
    let nr = lab.new_run;
    let name = file.name();
    let size = file.size() as u64;
    lab.scope.spawn(async move {
        match read_text(file).await {
            Ok(text) => nr.put_text(text, Some((name, size))),
            Err(e) => lab.toasts.err(e),
        }
    });
}

#[component]
pub(super) fn InputPart(lab: Lab) -> impl IntoView {
    let nr = lab.new_run;
    let over = RwSignal::new(false);
    let pick = move |f: BatchFormat| {
        nr.format_picked.set(true);
        nr.format.set(f);
    };
    view! {
        <section class="dl-newrun-part">
            <div class="dl-card-head">
                <h3 class="lab-h3">"File"</h3>
                <span class="spacer"></span>
                <span class="seg dl-format" title="CSV: RFC 4180 with a header row · JSONL: one object per line, its keys the columns">
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || nr.format.get() == BatchFormat::Csv
                        on:click=move |_| pick(BatchFormat::Csv)
                    >
                        "CSV"
                    </button>
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || nr.format.get() == BatchFormat::Jsonl
                        on:click=move |_| pick(BatchFormat::Jsonl)
                    >
                        "JSONL"
                    </button>
                </span>
            </div>
            {move || match nr.file.get() {
                Some((name, size)) => view! {
                    <div class="dl-file-chip">
                        <b class="n" title=name.clone()>{name.clone()}</b>
                        <span class="mini-note dim">{human_bytes(size)}</span>
                        <span class="spacer"></span>
                        <button
                            type="button"
                            class="btn ghost sm"
                            title="Drop the file and paste text instead"
                            on:click=move |_| nr.put_text(String::new(), None)
                        >
                            "✕ another file or paste"
                        </button>
                    </div>
                }
                .into_any(),
                None => view! {
                    <label
                        class="dl-drop"
                        class:over=move || over.get()
                        on:dragover=move |ev: web_sys::DragEvent| {
                            ev.prevent_default();
                            over.set(true);
                        }
                        on:dragleave=move |_| over.set(false)
                        on:drop=move |ev: web_sys::DragEvent| {
                            ev.prevent_default();
                            over.set(false);
                            take(lab, ev.data_transfer().and_then(|d| d.files()).and_then(|l| l.get(0)));
                        }
                    >
                        <input
                            type="file"
                            accept=".csv,.jsonl,.ndjson,.json,.txt,text/csv,application/json"
                            class="dl-file"
                            on:change=move |ev| {
                                let el: web_sys::HtmlInputElement = event_target(&ev);
                                take(lab, el.files().and_then(|l| l.get(0)));
                                el.set_value("");
                            }
                        />
                        <span>"Drop a CSV or JSONL file here, or "</span>
                        <span class="link">"pick one"</span>
                        <span>", or paste below"</span>
                    </label>
                    <textarea
                        class="input ta mono dl-paste"
                        placeholder="ticket_id,text,team\nT-1001,\"Charged twice for order A-1001\",billing"
                        prop:value=move || nr.text.get()
                        on:input=move |ev| nr.put_text(event_target_value(&ev), None)
                    ></textarea>
                }
                .into_any(),
            }}
        </section>
    }
}
