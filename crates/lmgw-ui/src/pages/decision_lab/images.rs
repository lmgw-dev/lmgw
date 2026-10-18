//! The images a decision request sends (labs-store design §2.3, §1.4): a
//! drop zone and a file picker, shown when a model asked reads images or
//! the form already holds some; thumbnails with their sizes and ✕. A picked
//! file is read into a data URL here and becomes the group's input artifact
//! on Ask; an image of an opened group is that artifact already, shown from
//! its file and sent by id.
//!
//! When the models asked differ, a line names those that read no images:
//! they get the images anyway, and answer llama.cpp's 501.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::ImageInput;
use wasm_bindgen_futures::JsFuture;

use super::form_state::Img;
use super::page::Lab;
use crate::fmt::human_bytes;

/// A picked file as a data URL, its type the browser's word for it.
async fn read_data_url(file: web_sys::File) -> Result<(String, u64), String> {
    let buf = JsFuture::from(file.array_buffer())
        .await
        .map_err(|_| format!("{} could not be read", file.name()))?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    let mime = match file.type_() {
        t if t.is_empty() => "application/octet-stream".to_string(),
        t => t,
    };
    let url = format!("data:{mime};base64,{}", lmgw_client::base64::encode(&bytes));
    Ok((url, bytes.len() as u64))
}

/// The models asked that read no images, for the line under the drop zone.
pub(super) fn blind(asked: &[(String, bool)]) -> Vec<String> {
    asked
        .iter()
        .filter(|(_, reads)| !reads)
        .map(|(n, _)| n.clone())
        .collect()
}

fn add_files(lab: Lab, files: Vec<web_sys::File>) {
    for file in files {
        lab.scope.spawn(async move {
            let name = file.name();
            match read_data_url(file).await {
                Ok((url, size)) => {
                    let uid = lab.form.uids.next();
                    lab.form.images.update(|v| {
                        v.push(Img {
                            uid,
                            src: url.clone(),
                            size,
                            head: None,
                            input: ImageInput::DataUrl { name, url },
                        })
                    });
                }
                Err(e) => lab.toasts.err(e),
            }
        });
    }
}

fn file_list(list: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(list) = list else {
        return Vec::new();
    };
    (0..list.length()).filter_map(|i| list.get(i)).collect()
}

#[component]
pub(super) fn ImagesCard(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let asked = Memo::new(move |_| {
        lab.asked()
            .into_iter()
            .map(|n| {
                let reads = lab.model_of(&n).is_some_and(|m| m.reads_images);
                (n, reads)
            })
            .collect::<Vec<_>>()
    });
    let shown =
        move || asked.with(|a| a.iter().any(|(_, r)| *r)) || f.images.with(|v| !v.is_empty());
    let over = RwSignal::new(false);
    view! {
        <Show when=shown>
            <div class="card dl-images">
                <h3 class="lab-h3">"Images"</h3>
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
                        let files = file_list(ev.data_transfer().and_then(|d| d.files()));
                        add_files(lab, files);
                    }
                >
                    <input
                        type="file"
                        accept="image/*"
                        multiple
                        class="dl-file"
                        on:change=move |ev| {
                            let el: web_sys::HtmlInputElement = event_target(&ev);
                            add_files(lab, file_list(el.files()));
                            el.set_value("");
                        }
                    />
                    <span>"Drop images here, or "</span>
                    <span class="link">"pick files"</span>
                </label>
                <div class="dl-thumbs">
                    <For each=move || f.images.get() key=|i| i.uid let:img>
                        {
                            let uid = img.uid;
                            let name = img.input.name().to_string();
                            let kept = matches!(img.input, ImageInput::Artifact { .. });
                            view! {
                                <figure class="dl-thumb">
                                    <img src=img.src.clone() alt=name.clone()/>
                                    <figcaption>
                                        <span class="n" title=name.clone()>
                                            {name.clone()}
                                        </span>
                                        <span class="dim mono-sm">
                                            {human_bytes(img.size)}
                                            {kept.then_some(" · kept")}
                                        </span>
                                        <button
                                            class="btn ghost sm del"
                                            title="Remove this image from the form"
                                            on:click=move |_| f.images.update(|v| v.retain(|i| i.uid != uid))
                                        >
                                            "✕"
                                        </button>
                                    </figcaption>
                                </figure>
                            }
                        }
                    </For>
                </div>
                {move || {
                    let a = asked.get();
                    let none = blind(&a);
                    let n = f.images.with(Vec::len);
                    (!none.is_empty() && (n > 0 || a.len() > 1))
                        .then(|| {
                            view! {
                                <p class="mini-note dl-warn-line">
                                    {format!(
                                        "{} {} no images: {} sent all the same, and llama.cpp answers 501.",
                                        none.join(", "),
                                        if none.len() == 1 { "reads" } else { "read" },
                                        if n == 1 { "the image is" } else { "the images are" },
                                    )}
                                </p>
                            }
                        })
                }}
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_that_read_no_images_are_named() {
        let asked = vec![
            ("decide/clef".to_string(), true),
            ("decide/laya".to_string(), false),
        ];
        assert_eq!(blind(&asked), ["decide/laya"]);
    }
}
