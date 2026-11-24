//! The Documents lab's form (WP12): the files, dropped or picked, in request
//! order with their type and size, and the native route's options in plain
//! words, grouped (output, pages and tables; text recognition; model help;
//! pictures). Every option is rendered and sent as set; none is hidden
//! because its feature is not built yet or its program is missing here.
//! Instead each group says what this computer's capability report
//! (`doc_tools`) says about it, and what the last run's answer said about
//! its options (a `not_yet`, `tool_missing` or `model_not_chosen` note), so
//! a feature another work package builds lights up without a change here.

use leptos::prelude::*;
use lmgw_api_types::documents::convert::{ConvertOptions, ConvertWarning};
use lmgw_api_types::documents::Quality;
use lmgw_api_types::documents_lab::{DocumentsLabRoute, DocumentsLabSettings};
use lmgw_api_types::lab_store::LabFileRef;
use serde_json::Value;
use wasm_bindgen::JsCast;

use super::super::lab_rows::FilePick;
use super::viewer::WarningRow;
use super::Lab;
use crate::fmt::fmt_bytes;
use crate::widgets::{ActionRow, ComboRow, EntryRow, Ico, Icon, PrefGroup, SwitchRow};

/// One file of the list.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Picked {
    /// The row's key, and the upload's in the page's file store.
    pub key: u32,
    pub name: String,
    pub size: u64,
    /// What it is, as far as the page can tell before the route reads it.
    pub kind: String,
    /// A file the lab store keeps (a re-run's): sent without an upload.
    pub kept: Option<LabFileRef>,
}

/// The option fields, as the form holds them.
#[derive(Clone, Copy)]
pub(super) struct Opts {
    pub output: RwSignal<String>,
    pub pages: RwSignal<String>,
    pub tables: RwSignal<bool>,
    pub compact_tables: RwSignal<bool>,
    pub page_break: RwSignal<String>,
    pub ocr: RwSignal<String>,
    pub ocr_languages: RwSignal<String>,
    /// Empty: the setting's.
    pub model_help: RwSignal<String>,
    pub pictures: RwSignal<String>,
    pub picture_min_area: RwSignal<String>,
    pub images_scale: RwSignal<String>,
    pub describe_pictures: RwSignal<bool>,
    pub picture_prompt: RwSignal<String>,
    pub run_async: RwSignal<bool>,
    pub docling: RwSignal<bool>,
}

fn wire<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn enum_of<T: serde::de::DeserializeOwned>(name: &str, s: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| format!("{name} '{s}' is not one the route takes"))
}

impl Opts {
    pub fn new() -> Self {
        let o = Self {
            output: RwSignal::new(String::new()),
            pages: RwSignal::new(String::new()),
            tables: RwSignal::new(true),
            compact_tables: RwSignal::new(false),
            page_break: RwSignal::new(String::new()),
            ocr: RwSignal::new(String::new()),
            ocr_languages: RwSignal::new(String::new()),
            model_help: RwSignal::new(String::new()),
            pictures: RwSignal::new(String::new()),
            picture_min_area: RwSignal::new(String::new()),
            images_scale: RwSignal::new(String::new()),
            describe_pictures: RwSignal::new(false),
            picture_prompt: RwSignal::new(String::new()),
            run_async: RwSignal::new(false),
            docling: RwSignal::new(false),
        };
        o.load(&DocumentsLabSettings::default());
        o
    }

    /// The form as the route reads it, or the field that does not read.
    pub fn settings(&self) -> Result<DocumentsLabSettings, String> {
        let some = |s: String| {
            let t = s.trim().to_string();
            (!t.is_empty()).then_some(t)
        };
        let area = self.picture_min_area.get();
        let picture_min_area = area
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| format!("the picture area '{area}' is not a number"))?;
        let scale = self.images_scale.get();
        let images_scale = scale
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| format!("the picture scale '{scale}' is not a number"))?;
        let langs: Vec<String> = self
            .ocr_languages
            .get()
            .split([',', '+', ' '])
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let help = self.model_help.get();
        let options = ConvertOptions {
            output: enum_of("output", &self.output.get())?,
            pages: some(self.pages.get()),
            model_help: if help.is_empty() {
                None
            } else {
                Some(enum_of("model help", &help)?)
            },
            ocr: enum_of("text recognition", &self.ocr.get())?,
            ocr_languages: (!langs.is_empty()).then_some(langs),
            // The form has no row for a deadline per file yet.
            document_timeout: None,
            tables: self.tables.get(),
            pictures: enum_of("pictures", &self.pictures.get())?,
            picture_min_area,
            images_scale,
            describe_pictures: self.describe_pictures.get(),
            picture_prompt: some(self.picture_prompt.get()),
            // The form has no row for the least area described yet.
            describe_min_area: lmgw_api_types::documents::convert::DESCRIBE_MIN_AREA_DEFAULT,
            page_break: some(self.page_break.get()),
            compact_tables: self.compact_tables.get(),
            run_async: self.run_async.get(),
        };
        Ok(DocumentsLabSettings {
            route: if self.docling.get() {
                DocumentsLabRoute::Docling
            } else {
                DocumentsLabRoute::Native
            },
            options,
        })
    }

    /// Put `s` into the form.
    pub fn load(&self, s: &DocumentsLabSettings) {
        let o = &s.options;
        self.output.set(wire(&o.output));
        self.pages.set(o.pages.clone().unwrap_or_default());
        self.tables.set(o.tables);
        self.compact_tables.set(o.compact_tables);
        self.page_break
            .set(o.page_break.clone().unwrap_or_default());
        self.ocr.set(wire(&o.ocr));
        self.ocr_languages
            .set(o.ocr_languages.clone().unwrap_or_default().join(","));
        self.model_help
            .set(o.model_help.map(|m| wire(&m)).unwrap_or_default());
        self.pictures.set(wire(&o.pictures));
        self.picture_min_area.set(o.picture_min_area.to_string());
        self.images_scale.set(o.images_scale.to_string());
        self.describe_pictures.set(o.describe_pictures);
        self.picture_prompt
            .set(o.picture_prompt.clone().unwrap_or_default());
        self.run_async.set(o.run_async);
        self.docling.set(s.route == DocumentsLabRoute::Docling);
    }
}

/// What a file is from its first bytes and its name: what the list shows
/// before the route reads it (the route reads the bytes, never the name).
pub(super) fn sniff(head: &[u8], name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    if head.starts_with(b"%PDF") {
        return "PDF".into();
    }
    if head.starts_with(b"PK\x03\x04") {
        return match ext {
            "docx" => "Word",
            "xlsx" => "Excel",
            "pptx" => "PowerPoint",
            "odt" | "ods" | "odp" => "OpenDocument",
            "epub" => "EPUB",
            _ => "ZIP archive",
        }
        .into();
    }
    if head.starts_with(b"\x89PNG") {
        return "PNG picture".into();
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "JPEG picture".into();
    }
    if head.starts_with(b"\xD0\xCF\x11\xE0") {
        return "old Office file".into();
    }
    if std::str::from_utf8(head).is_ok() || head.is_empty() {
        return match ext {
            "md" | "markdown" => "Markdown",
            "html" | "htm" => "web page",
            "csv" => "CSV",
            _ => "text",
        }
        .into();
    }
    "unknown".into()
}

/// The type a kept file's stored MIME type says, in the list's words.
pub(super) fn kind_of_mime(mime: &str, name: &str) -> String {
    match mime.split(';').next().unwrap_or_default().trim() {
        "application/pdf" => "PDF".into(),
        "image/png" => "PNG picture".into(),
        "image/jpeg" => "JPEG picture".into(),
        m if m.starts_with("text/") => sniff(b"", name),
        m if m.contains("officedocument") || m.contains("opendocument") => {
            sniff(b"PK\x03\x04", name)
        }
        m => m.to_string(),
    }
}

/// `files` with the one at `i` moved by `by` places (−1 up, +1 down).
pub(super) fn moved(mut files: Vec<Picked>, i: usize, by: isize) -> Vec<Picked> {
    let j = i as isize + by;
    if i < files.len() && j >= 0 && (j as usize) < files.len() {
        files.swap(i, j as usize);
    }
    files
}

/// The last run's notes on the options `names`.
fn notes_for(lab: Lab, names: &'static [&'static str]) -> impl IntoView {
    move || {
        lab.notes
            .with(|n| {
                n.iter()
                    .filter(|w| w.option.as_deref().is_some_and(|o| names.contains(&o)))
                    .cloned()
                    .collect::<Vec<ConvertWarning>>()
            })
            .into_iter()
            .map(|w| view! { <div class="doclab-optnote"><WarningRow w=w/></div> })
            .collect_view()
    }
}

/// What the capability report says about the format rows `ids` when they
/// are not read in full here.
fn caps_note(lab: Lab, ids: &'static [&'static str]) -> impl IntoView {
    move || {
        lab.caps
            .with(|c| {
                c.as_ref()
                    .map(|c| {
                        c.formats
                            .iter()
                            .filter(|f| ids.contains(&f.id.as_str()) && f.quality != Quality::Full)
                            .map(|f| format!("{} ({})", f.label, f.said))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .into_iter()
            .map(|s| view! { <p class="pref-desc doclab-caps">{s}</p> })
            .collect_view()
    }
}

fn opts_list(pairs: &[(&str, &str)]) -> Signal<Vec<(String, String)>> {
    let v: Vec<(String, String)> = pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    Signal::derive(move || v.clone())
}

/// The files: the drop zone and picker, then the list.
#[component]
fn FilesCard(lab: Lab) -> impl IntoView {
    let over = RwSignal::new(false);
    let add_list = move |list: Option<web_sys::FileList>| {
        let Some(list) = list else { return };
        let files: Vec<web_sys::File> = (0..list.length()).filter_map(|i| list.get(i)).collect();
        lab.add_files(files);
    };
    let on_pick = Callback::new(move |ev: web_sys::Event| {
        let el: web_sys::HtmlInputElement = event_target(&ev);
        add_list(el.files());
        el.set_value("");
    });
    let on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        over.set(false);
        add_list(ev.data_transfer().and_then(|d| d.files()));
    };
    let total = move || {
        lab.files.with(|f| {
            let bytes: u64 = f.iter().map(|p| p.size).sum();
            match f.len() {
                0 => "No files yet".to_string(),
                1 => format!("1 file · {}", fmt_bytes(bytes)),
                n => format!("{n} files · {} · sent in this order", fmt_bytes(bytes)),
            }
        })
    };
    view! {
        <PrefGroup
            title="Files"
            desc="Drop files here or pick them. The route reads each file's type from its bytes, never from its name."
        >
            <div
                class="doclab-drop"
                class:over=move || over.get()
                on:dragover=move |ev: web_sys::DragEvent| {
                    ev.prevent_default();
                    over.set(true);
                }
                on:dragleave=move |_| over.set(false)
                on:drop=on_drop
            >
                <Icon ico=Ico::Upload/>
                <span>{total}</span>
                <span class="spacer"></span>
                <FilePick accept="*/*" label="Add files…" multiple=true title="Pick one or several files" on_file=on_pick/>
            </div>
            <For
                each=move || {
                    let n = lab.files.with(Vec::len);
                    lab.files.get().into_iter().enumerate().map(move |(i, f)| (i, n, f)).collect::<Vec<_>>()
                }
                key=|(i, _, f)| (f.key, *i, f.kind.clone())
                let:item
            >
                {
                    let (i, n, f) = item;
                    let kept = f.kept.is_some();
                    let sub = format!(
                        "{} · {}{}",
                        f.kind,
                        fmt_bytes(f.size),
                        if kept { " · kept from an earlier run" } else { "" }
                    );
                    view! {
                        <ActionRow title=f.name.clone() sub=sub>
                            <button
                                class="icon-btn"
                                title="Move up: files are sent in this order"
                                aria-label="Move up"
                                disabled=i == 0
                                on:click=move |_| lab.files.update(|v| *v = moved(std::mem::take(v), i, -1))
                            >
                                "▲"
                            </button>
                            <button
                                class="icon-btn"
                                title="Move down"
                                aria-label="Move down"
                                disabled={i + 1 >= n}
                                on:click=move |_| lab.files.update(|v| *v = moved(std::mem::take(v), i, 1))
                            >
                                "▼"
                            </button>
                            <button
                                class="icon-btn"
                                title="Remove from the list"
                                aria-label="Remove"
                                on:click=move |_| lab.remove_file(i)
                            >
                                <Icon ico=Ico::Close/>
                            </button>
                        </ActionRow>
                    }
                }
            </For>
        </PrefGroup>
    }
}

/// The whole form column.
#[component]
pub(super) fn FormView(lab: Lab) -> impl IntoView {
    let o = lab.opts;
    let route_note = move || {
        if o.docling.get() {
            "Sent to /v1/convert/file as a docling-serve client sends them: each option below as the docling option lmgw maps it onto. One file answers docling's JSON, several the converted_docs.zip."
        } else {
            "Sent to /v1/documents/convert, lmgw's own route, with every option below that differs from its default."
        }
    };
    let unsent = move || {
        o.settings()
            .ok()
            .and_then(|s| s.fields().ok())
            .map(|(_, u)| u)
            .unwrap_or_default()
    };
    view! {
        <FilesCard lab=lab/>
        <PrefGroup title="Route" desc=move || route_note().to_string()>
            <SwitchRow
                title="Background job"
                sub="Answer at once with a job and follow it here; native route only"
                value=o.run_async
                wire="async"
            />
            {move || {
                let u = unsent();
                (!u.is_empty())
                    .then(|| {
                        view! {
                            <p class="pref-desc doclab-caps">
                                {format!("Not sent on this route (docling's form has no field for it): {}", u.join(", "))}
                            </p>
                        }
                    })
            }}
        </PrefGroup>
        <PrefGroup title="Output, pages and tables">
            <ComboRow
                title="Output"
                sub="Markdown keeps headings, lists and tables; text leaves the markup out; structure gives parts and blocks"
                value=o.output
                options=opts_list(&[("markdown", "Markdown"), ("text", "Plain text"), ("json", "Structure (JSON)")])
                wire="output"
            />
            <EntryRow title="Pages" sub="Page numbers, as 3-7,10; empty reads every page" value=o.pages placeholder="every page" wire="pages"/>
            <EntryRow title="Page break" sub="A line written between two pages" value=o.page_break placeholder="none" wire="page_break"/>
            <SwitchRow title="Tables" sub="Off writes each row as cells separated by tabs" value=o.tables wire="tables"/>
            <SwitchRow title="Compact tables" sub="Markdown tables without the padding" value=o.compact_tables wire="compact_tables"/>
            {notes_for(lab, &["output", "pages", "page_break", "tables", "compact_tables"])}
        </PrefGroup>
        <PrefGroup title="Text recognition">
            {caps_note(lab, &["scans", "pictures"])}
            <ComboRow
                title="Read scanned pages"
                value=o.ocr
                options=opts_list(&[("auto", "Pages without text"), ("off", "Off"), ("force", "Every page")])
                wire="ocr"
            />
            <EntryRow title="Languages" sub="Language codes, as eng,deu" value=o.ocr_languages placeholder="From Settings" wire="ocr_languages"/>
            {notes_for(lab, &["ocr", "ocr_languages"])}
        </PrefGroup>
        <PrefGroup title="Model help">
            <ComboRow
                title="Read pages with a model"
                sub="Asked last, after the text layer and text recognition: the pages they left unread or read poorly"
                value=o.model_help
                options=opts_list(&[
                    ("", "From Settings"),
                    ("off", "Off"),
                    ("missing", "Pages nothing else read"),
                    ("missing_or_tables", "Unread or poorly read pages, and tables"),
                    ("every_page", "Every page"),
                ])
                wire="model_help"
            />
            {notes_for(lab, &["model_help"])}
        </PrefGroup>
        <PrefGroup title="Pictures">
            {caps_note(lab, &["pictures", "images", "picture_descriptions"])}
            <ComboRow
                title="Pictures in the text"
                value=o.pictures
                options=opts_list(&[("placeholder", "A placeholder"), ("embedded", "Embedded"), ("referenced", "Referenced files")])
                wire="pictures"
            />
            <EntryRow title="Smallest picture" sub="Its least share of the page's area" value=o.picture_min_area wire="picture_min_area"/>
            <EntryRow title="Picture scale" sub="PDF pictures are cut out at 72 dots per inch times this" value=o.images_scale wire="images_scale"/>
            <SwitchRow title="Describe pictures" sub="One short description per picture, by the model chosen under Settings › Documents" value=o.describe_pictures wire="describe_pictures"/>
            <EntryRow title="Description prompt" value=o.picture_prompt placeholder="lmgw's default prompt" wire="picture_prompt"/>
            {notes_for(lab, &["pictures", "picture_min_area", "images_scale", "describe_pictures", "picture_prompt"])}
        </PrefGroup>
    }
}

/// A file's first bytes, for [`sniff`].
pub(super) async fn head_of(file: &web_sys::File) -> Vec<u8> {
    let blob = file.slice_with_i32_and_i32(0, 16);
    let Ok(blob) = blob else { return Vec::new() };
    let buf = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await;
    match buf {
        Ok(b) => js_sys::Uint8Array::new(&b.unchecked_into()).to_vec(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_told_by_its_bytes_first() {
        assert_eq!(sniff(b"%PDF-1.7", "report.docx"), "PDF");
        assert_eq!(sniff(b"PK\x03\x04xx", "notes.docx"), "Word");
        assert_eq!(sniff(b"PK\x03\x04xx", "a.bin"), "ZIP archive");
        assert_eq!(sniff(b"# hello", "a.md"), "Markdown");
        assert_eq!(sniff(&[0xFF, 0xFE, 0x00, 0xD8], "a.txt"), "unknown");
        assert_eq!(kind_of_mime("application/pdf", "x"), "PDF");
    }

    #[test]
    fn a_file_moves_within_the_list_only() {
        let f = |k| Picked {
            key: k,
            name: k.to_string(),
            size: 1,
            kind: String::new(),
            kept: None,
        };
        let v = moved(vec![f(1), f(2), f(3)], 0, 1);
        assert_eq!(v.iter().map(|p| p.key).collect::<Vec<_>>(), [2, 1, 3]);
        let v = moved(v, 0, -1);
        assert_eq!(v.iter().map(|p| p.key).collect::<Vec<_>>(), [2, 1, 3]);
        let v = moved(v, 2, 1);
        assert_eq!(v.iter().map(|p| p.key).collect::<Vec<_>>(), [2, 1, 3]);
    }
}
