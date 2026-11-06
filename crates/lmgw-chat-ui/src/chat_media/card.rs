//! The image card (design §8.2): a produced picture with its facts, and the
//! pictures card actions made from it as a strip of thumbnails under it,
//! the newest selected. Each picture of the strip has its own facts and
//! actions (`actions`).

use leptos::prelude::*;
use lmgw_api_types::chat_media::MediaFacts;
use lmgw_api_types::chat_threads::AttachmentMeta;

use super::actions::{ActionBar, Runner};
use super::live::LivePane;
use super::{facts, family_of, secs, use_media};
use crate::chat_bytes::{attachment_path, ChatImg};

/// The facts line: model (and the fallback that answered), seed, steps,
/// cfg, size, time.
pub(super) fn fact_words(f: &MediaFacts) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(m) = &f.model {
        v.push(m.clone());
    }
    if let Some(u) = &f.upscaler {
        v.push(match f.scale {
            Some(s) => format!("upscaled ×{s} with {u}"),
            None => format!("upscaled with {u}"),
        });
    }
    if let Some(s) = f.seed {
        v.push(format!("seed {s}"));
    }
    if let Some(s) = f.steps {
        v.push(format!("{s} steps"));
    }
    if let Some(c) = f.cfg {
        v.push(format!("cfg {c}"));
    }
    if let Some(s) = f.strength {
        v.push(format!("strength {s}"));
    }
    if let (Some(w), Some(h)) = (f.width, f.height) {
        v.push(format!("{w}×{h}"));
    }
    match (f.job_ms, f.wall_ms) {
        (Some(job), Some(wall)) if wall >= job + 100 => {
            v.push(format!("{}, plus {} loading", secs(job), secs(wall - job)));
        }
        (Some(job), _) => v.push(secs(job)),
        (None, Some(wall)) => v.push(secs(wall)),
        (None, None) => {}
    }
    v
}

/// The picture `root` of the message's `files` and every action picture
/// made from it. `settled`: the message is stored (the actions need it).
#[component]
pub(super) fn ImageFamily(
    root: i64,
    files: RwSignal<Vec<AttachmentMeta>>,
    #[prop(into)] settled: Signal<bool>,
) -> impl IntoView {
    let ctx = use_media();
    let family = Memo::new(move |_| files.with(|f| family_of(f, root)));
    let family_ids =
        Memo::new(move |_| family.with(|f| f.iter().map(|a| a.id).collect::<Vec<_>>()));
    // The picture the owner picked in the strip; none is the newest.
    let picked = RwSignal::new(None::<i64>);
    let shown = Memo::new(move |_| {
        let p = picked.get();
        family.with(|fam| {
            p.and_then(|p| fam.iter().find(|a| a.id == p).cloned())
                .or_else(|| fam.last().cloned())
        })
    });
    let runner = Runner::new(files, picked, family_ids);
    let shown_facts = Memo::new(move |_| shown.with(|a| a.as_ref().map(facts).unwrap_or_default()));
    let aspect = Signal::derive(move || shown_facts.with(|f| f.width.zip(f.height)));
    let frame_style = move || {
        let (w, h) = aspect.get().unwrap_or((1, 1));
        super::frame_style(w, h)
    };
    let strip = move || family.with(Vec::len) > 1;
    view! {
        <div class="media-card" data-media-root=root.to_string()>
            {move || {
                shown
                    .get()
                    .map(|a| {
                        let open_a = a.clone();
                        // What the picture shows is its prompt; a picture
                        // without one (an upscale's) is said by its name.
                        let alt = facts(&a)
                            .prompt
                            .filter(|p| !p.trim().is_empty())
                            .unwrap_or_else(|| a.name.clone());
                        view! {
                            <button
                                class="media-frame media-pic"
                                style=frame_style
                                title="Open full size"
                                data-attachment=a.id.to_string()
                                on:click=move |_| ctx.open.run(open_a.clone())
                            >
                                <ChatImg path=attachment_path(a.id) alt=alt/>
                            </button>
                        }
                    })
            }}
            <div class="media-strip-row">
            <Show when=strip>
                <div class="media-strip" role="listbox" aria-label="Pictures made from this one">
                    <For each=move || family.get() key=|a| a.id let:a>
                        {
                            let id = a.id;
                            view! {
                                <button
                                    class="media-thumb"
                                    role="option"
                                    aria-selected=move || {
                                        (shown.with(|s| s.as_ref().map(|s| s.id)) == Some(id))
                                            .to_string()
                                    }
                                    title=a.name.clone()
                                    data-attachment=id.to_string()
                                    on:click=move |_| picked.set(Some(id))
                                >
                                    <ChatImg path=attachment_path(id) alt=""/>
                                </button>
                            }
                        }
                    </For>
                </div>
            </Show>
            // A running action's pane beside the strip, not in its list.
            <Show when=move || runner.busy()>
                <LivePane
                    live=Signal::derive(move || runner.live())
                    call_id=Signal::derive(move || runner.call_id())
                    aspect=aspect
                    what=Signal::derive(move || runner.what())
                    compact=true
                />
            </Show>
            </div>
            <div class="media-facts dim mono-sm">
                {move || shown_facts.with(fact_words).join(" · ")}
                {move || {
                    shown_facts
                        .with(|f| f.answered_by.clone())
                        .map(|b| {
                            view! {
                                <span class="media-fallback">
                                    {format!("answered by {b}, fallback")}
                                </span>
                            }
                        })
                }}
            </div>
            {move || {
                shown_facts
                    .with(|f| (!f.not_applied.is_empty()).then(|| f.not_applied.join(", ")))
                    .map(|n| {
                        view! { <div class="media-facts dim mini-note">{format!("not applied: {n}")}</div> }
                    })
            }}
            {move || {
                shown_facts
                    .with(|f| f.prompt.clone())
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| view! { <div class="media-prompt" title=p.clone()>{p.clone()}</div> })
            }}
            {move || shown.get().map(|a| view! { <ActionBar a=a runner=runner settled=settled/> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_read_as_a_line() {
        let f = MediaFacts {
            model: Some("image/z".into()),
            seed: Some(7),
            steps: Some(8),
            cfg: Some(1.0),
            width: Some(512),
            height: Some(512),
            job_ms: Some(1100),
            wall_ms: Some(4700),
            ..Default::default()
        };
        assert_eq!(
            fact_words(&f).join(" · "),
            "image/z · seed 7 · 8 steps · cfg 1 · 512×512 · 1.1 s, plus 3.6 s loading"
        );
        let cloud = MediaFacts {
            model: Some("image/z".into()),
            wall_ms: Some(900),
            ..Default::default()
        };
        assert_eq!(fact_words(&cloud).join(" · "), "image/z · 0.9 s");
    }
}
