//! The response half of the tester (api-docs design §6.8): the status chip,
//! the timing line, the headers table (`x-lmgw-*` rows first, highlighted),
//! and the body — JSON, plain text, or a Blob URL played/downloaded by its
//! content type. The live SSE view is `stream.rs`'s frames rendered by
//! `tester.rs` directly, since they arrive over time rather than once here.

use leptos::prelude::*;

use super::draft::FinishedKind;
use super::json_view::JsonView;

/// `.chip.resp-status` class suffix for a status code (§6.8): by class —
/// 2xx ok, a 3xx/4xx warn, a 5xx or no response at all err.
pub fn status_class(status: u16) -> &'static str {
    match status {
        200..=299 => "ok",
        0 | 500..=599 => "err",
        _ => "warn",
    }
}

/// "headers in X ms · done in Y ms"; a stream still open says so instead.
pub fn timing_line(headers_ms: f64, done_ms: Option<f64>) -> String {
    match done_ms {
        Some(done) => format!("headers in {headers_ms:.0} ms · done in {done:.0} ms"),
        None => format!("headers in {headers_ms:.0} ms · streaming…"),
    }
}

#[component]
pub fn StatusChip(status: u16) -> impl IntoView {
    let cls = format!("chip resp-status {}", status_class(status));
    let label = if status == 0 {
        "no response".to_string()
    } else {
        status.to_string()
    };
    view! {
        <span class=cls>
            <span class="dot"></span>
            {label}
        </span>
    }
}

/// `x-lmgw-*` rows first (`tr.lmgw`, highlighted per §6.10), everything else
/// after in the order the response sent them.
#[component]
pub fn HeadersTable(headers: Vec<(String, String)>) -> impl IntoView {
    let mut sorted = headers;
    sorted.sort_by_key(|(k, _)| !k.to_ascii_lowercase().starts_with("x-lmgw-"));
    view! {
        <table class="data kv-table resp-headers">
            <tbody>
                <For each=move || sorted.clone() key=|(k, v)| (k.clone(), v.clone()) let:row>
                    {
                        let (name, value) = row;
                        let lmgw = name.to_ascii_lowercase().starts_with("x-lmgw-");
                        view! {
                            <tr class:lmgw=lmgw>
                                <td class="mono-sm">{name}</td>
                                <td class="mono-sm wrap">{value}</td>
                            </tr>
                        }
                    }
                </For>
            </tbody>
        </table>
    }
}

/// The body (§6.7's classification). An SSE body's frames are rendered live
/// by `tester.rs` off `stream.rs`'s splitter; this only says how it ended.
#[component]
pub fn ResponseBody(outcome: FinishedKind) -> impl IntoView {
    match outcome {
        FinishedKind::Sse { error: None } => ().into_any(),
        FinishedKind::Sse { error: Some(e) } => {
            view! { <p class="field-warn">"stream ended early: " {e}</p> }.into_any()
        }
        FinishedKind::Empty => view! { <p class="dim">"(no body)"</p> }.into_any(),
        FinishedKind::Json(v) => {
            view! { <div class="resp-json"><JsonView value=v/></div> }.into_any()
        }
        FinishedKind::Text(t) => view! { <pre class="resp-text">{t}</pre> }.into_any(),
        FinishedKind::Blob {
            url,
            content_type,
            filename,
            size,
        } => {
            let bytes = crate::fmt::human_bytes(size as u64);
            let player = if content_type.starts_with("audio/") {
                Some(
                    view! { <audio controls class="resp-audio" src=url.clone()></audio> }
                        .into_any(),
                )
            } else if content_type.starts_with("image/") {
                Some(
                    view! { <img class="resp-image" src=url.clone() alt=filename.clone()/> }
                        .into_any(),
                )
            } else {
                None
            };
            view! {
                <div class="resp-blob">
                    {player}
                    <a class="resp-download btn ghost sm" href=url download=filename.clone()>
                        {format!("Download {filename} ({bytes})")}
                    </a>
                </div>
            }
            .into_any()
        }
        FinishedKind::Error(e) => view! { <p class="notice err">{e}</p> }.into_any(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_read_ok_warn_err() {
        assert_eq!(status_class(200), "ok");
        assert_eq!(status_class(204), "ok");
        assert_eq!(status_class(302), "warn");
        assert_eq!(status_class(401), "warn");
        assert_eq!(status_class(500), "err");
        assert_eq!(status_class(0), "err");
    }

    #[test]
    fn timing_reads_headers_then_done() {
        assert_eq!(
            timing_line(12.4, Some(88.9)),
            "headers in 12 ms · done in 89 ms"
        );
        assert_eq!(timing_line(12.4, None), "headers in 12 ms · streaming…");
    }
}
