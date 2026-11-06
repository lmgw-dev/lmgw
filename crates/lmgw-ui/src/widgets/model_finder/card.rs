//! The card (find-a-model design §5): one model and one file, its fit in
//! everyday words, the other measure's pick (D120), the download, the
//! licence, the lines of the machine, the hold, the card's memory and the
//! disk (D9, D10, D11, D125), "Why this file", "Choose the file myself"
//! (D34) and "Get it", which turns into its progress (§6.1).

use leptos::prelude::*;
use lmgw_api_types::model_find::{FileFit, FindAnswer, FitMark, FitMeasure, ModelCard};

use super::share::{PrivateLine, SendToFriend};
use super::shown::{self, Choice, Shown};
use super::words::{self, Checked};
use super::{no_card, progress, Finder, Getting};
use crate::pages::wizard::marks;
use crate::pages::wizard::{HfWizard, PickedFile};
use crate::widgets::machine_check::MachineInCard;
use crate::widgets::Modal;

/// Who makes room for a fit on the whole card, by the card's reading or,
/// before the marks, by what the fit says is beside it.
fn who(s: &Shown) -> &'static str {
    match &s.card {
        Some(c) => marks::who(Some(c)),
        None => {
            let b = s
                .fit
                .beside
                .as_ref()
                .or(s.other.as_ref().and_then(|o| o.fit.beside.as_ref()));
            marks::who_of(
                b.and_then(|b| b.outside_bytes),
                b.is_some_and(|b| b.residents_bytes > 0),
            )
        }
    }
}

/// The fit's notes the fold shows: the hold's line is the card's own.
fn fold_notes(fit: &FileFit, list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in fit.notes.iter().chain(list) {
        if n != words::HOLD_LINE && !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

#[component]
pub(super) fn FinderCard(f: Finder, card: ModelCard, answer: FindAnswer) -> impl IntoView {
    let card = StoredValue::new(card);
    // The file Get it got, kept while it runs and after it stopped: marks
    // that come in after Get it don't switch the card to another file.
    let frozen = RwSignal::new(None::<Shown>);
    let shown = Memo::new(move |_| {
        if let Some(s) = frozen.get() {
            return s;
        }
        card.with_value(|c| shown::shown(c, f.marks.get().as_ref(), &f.choice.get()))
    });
    let checked = Memo::new(move |_| {
        if shown.with(|s| s.exact) {
            Checked::Header
        } else if f.marks_busy.get() && frozen.with(Option::is_none) {
            Checked::Checking
        } else {
            Checked::BySize
        }
    });
    let live = crate::live::use_live();
    let held = move || {
        live.vram
            .with(|v| v.as_ref().is_some_and(|v| v.hold_active))
            || shown.with(|s| s.fit.notes.iter().any(|n| n == words::HOLD_LINE))
    };
    let kind = card.with_value(|c| c.kind);
    let has_file = card.with_value(|c| !c.upload.is_empty() && !c.file.is_empty());
    let nothing_fits = answer == FindAnswer::NothingFits;

    let head = move || {
        let s = shown.get();
        let (text, tone) = words::chip(&s.fit, who(&s), checked.get());
        let tip = marks::tip(&s.fit, None);
        view! { <span class=format!("chip {tone} mf-mark") title=tip>{text}</span> }
    };
    let by = card.with_value(|c| match &c.by_user {
        Some(u) if *u != c.by => format!("by {} ({u})", c.by),
        _ => format!("by {}", c.by),
    });
    let can = move || {
        let s = shown.get();
        let line = card.with_value(|c| words::can_line(&c.can, s.parameters));
        (!line.is_empty()).then(|| view! { <p class="mf-can">{line}</p> })
    };
    let ctx_line = move || words::context_line(kind, &shown.with(|s| s.fit.clone()), checked.get());
    let uses = move || {
        let s = shown.get();
        words::uses_line(&s.fit).map(|l| {
            view! {
                <p class="mf-line" title=marks::tip(&s.fit, None)>{l}</p>
            }
        })
    };
    // D120: the other measure's pick in one line, with the switch to it.
    let other = move || {
        let s = shown.get();
        let o = s.other.clone()?;
        if s.by_hand {
            return None;
        }
        // Said by the other file's own measure (WP7, D147), not by which
        // of the two is chosen: a pasted file may be either pick or neither.
        let lead = match o.fit.measure {
            FitMeasure::Beside => "Beside what runs now".to_string(),
            FitMeasure::WholeCard => format!("Once {}", who(&s)),
        };
        let lead = {
            let mut l = lead;
            if let Some(c) = l.get(..1) {
                l = c.to_uppercase() + &l[1..];
            }
            l
        };
        let words = words::other_words(o.quant.as_deref(), &o.file, &o.fit);
        Some(view! {
            <p class="mf-line mf-other">
                {format!("{lead}: {words} · ")}
                <button
                    class="link-btn"
                    disabled=move || !matches!(f.get.get(), Getting::Idle)
                    on:click=move |_| {
                        f.disk_ask.set(false);
                        f.choice.update(|c| {
                            *c = if *c == Choice::Other { Choice::Pick } else { Choice::Other };
                        });
                    }
                >
                    "Use this one instead"
                </button>
            </p>
        })
    };
    let download = move || {
        let s = shown.get();
        let (est, by_size_bytes) =
            card.with_value(|c| (c.download.clone(), c.download.files_bytes));
        let bytes = s.bytes.unwrap_or(by_size_bytes);
        let secs = if s.bytes == Some(by_size_bytes) {
            est.secs
        } else {
            words::secs_at(bytes, est.rate_bytes_per_sec)
        };
        let line = words::download_line(bytes, secs, est.engine_bytes);
        view! { <p class="mf-line" title=est.notes.join("\n")>{line}</p> }
    };
    let licence = card.with_value(|c| {
        let page = format!("https://huggingface.co/{}", c.origin);
        let (label, link) = match &c.licence {
            Some(l) => (
                l.label()
                    .map_or_else(|| "see the model's page".to_string(), str::to_string),
                l.link.clone().unwrap_or(page),
            ),
            None => ("see the model's page".to_string(), page),
        };
        view! {
            <p class="mf-line">
                "Licence: "
                <a href=link target="_blank" rel="noopener noreferrer">{label} " ↗"</a>
            </p>
        }
    });
    // D125 before a get (D144): the card counts the whole download. Once a
    // get of it stopped, part of it is on the disk, which only the server
    // counts: its answer's disk line shows under the progress.
    let disk = move || {
        if matches!(f.get.get(), Getting::Failed(_) | Getting::Canceled) {
            return None;
        }
        let s = shown.get();
        let free = card.with_value(|c| c.disk_free_bytes);
        words::disk_short(s.bytes.unwrap_or(0), free)
    };
    let metered = move || f.marks.with(|m| m.as_ref().and_then(|m| m.metered.clone()));
    let upload_target = card.with_value(|c| (c.upload.clone(), words::target_of(c.kind)));
    let check_exactly = {
        let (upload, target) = upload_target.clone();
        move |_| f.ask_marks(upload.clone(), target, "true")
    };

    // "Choose the file myself" (D34): the file list on the upload, its pick
    // handed back to the card.
    let pick_repo = RwSignal::new(String::new());
    let open_list = move |repo: String| {
        pick_repo.set(repo);
        f.picking.set(true);
    };
    let on_pick = Callback::new(move |p: PickedFile| {
        f.picking.set(false);
        f.disk_ask.set(false);
        // Another file: the stopped get's file and its words go.
        frozen.set(None);
        if matches!(
            f.get.get_untracked(),
            Getting::Failed(_) | Getting::Canceled
        ) {
            f.get.set(Getting::Idle);
        }
        f.choice.set(Choice::ByHand(Box::new(p)));
    });
    let fold = move || {
        let s = shown.get();
        let list_notes = f
            .marks
            .with(|m| m.as_ref().map(|m| m.fit_notes.clone()).unwrap_or_default());
        let notes = fold_notes(&s.fit, &list_notes);
        let read = f
            .marks
            .with(|m| m.as_ref().map_or(0, |m| m.header_bytes_read));
        let companions = card.with_value(|c| c.companions.clone());
        let uploads = card.with_value(|c| {
            (
                c.other_uploads.clone(),
                c.other_uploads_more,
                c.more_uploads,
            )
        });
        let header = if s.exact {
            if read > 0 {
                format!(
                    "lmgw read {} of the file's header to check this.",
                    crate::fmt::fmt_bytes(read)
                )
            } else {
                "lmgw checked this from the file's header, read earlier.".to_string()
            }
        } else {
            "lmgw hasn't read the file's header: the marks are by its size.".to_string()
        };
        view! {
            <details class="mf-fold">
                <summary>
                    "Why this file · "
                    <span class="mono">{words::short_file(&s.file)}</span>
                    {(!s.upload.is_empty()).then(|| format!(" from {}", s.upload))}
                </summary>
                <div class="mf-fold-body">
                    <p>{s.why.clone()}</p>
                    {s.full_context.clone().map(|c| view! { <p>{c}</p> })}
                    <p class="mono dim">{format!("{} · {}", s.upload, s.file)}</p>
                    {(!companions.is_empty()).then(|| view! {
                        <p>
                            "Downloaded with it: "
                            {companions
                                .iter()
                                .map(|c| format!("{} ({})", words::short_file(&c.file), match c.role.as_str() {
                                    "mmproj" => "reads pictures",
                                    "drafter" => "answers faster",
                                    other => other,
                                }))
                                .collect::<Vec<_>>()
                                .join(", ")}
                        </p>
                    })}
                    {s.bits_per_weight.map(|b| view! {
                        <p>{format!("{b:.1} bits per weight")}</p>
                    })}
                    <p class="dim">{header}</p>
                    {(!notes.is_empty()).then(|| view! {
                        <ul class="mf-notes-list">
                            {notes.into_iter().map(|n| view! { <li>{n}</li> }).collect_view()}
                        </ul>
                    })}
                    {(!uploads.0.is_empty()).then(|| {
                        let (list, more, more_hub) = uploads.clone();
                        view! {
                            <div class="mf-uploads">
                                <span class="dim">"Other uploads of this model: "</span>
                                {list
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, u)| {
                                        let repo = u.repo.clone();
                                        view! {
                                            {(i > 0).then_some(" · ")}
                                            <button
                                                class="link-btn"
                                                title="Open its file list"
                                                on:click=move |_| open_list(repo.clone())
                                            >
                                                {u.repo.clone()}
                                            </button>
                                        }
                                    })
                                    .collect_view()}
                                {(more > 0 || more_hub).then(|| view! {
                                    <span class="dim">
                                        {if more > 0 {
                                            format!(" · and {more} more, not listed here yet")
                                        } else {
                                            " · and more on Hugging Face".to_string()
                                        }}
                                    </span>
                                })}
                            </div>
                        }
                    })}
                </div>
            </details>
        }
    };

    let can_get = move || {
        has_file
            && matches!(
                f.get.get(),
                Getting::Idle | Getting::Failed(_) | Getting::Canceled
            )
    };
    let fits = move || {
        shown.with(|s| !matches!(s.fit.mark, FitMark::DoesNotFit | FitMark::DoesNotFitBySize))
    };
    let get_it = move |_| {
        if !has_file {
            return;
        }
        let s = shown.get_untracked();
        if disk().is_some() && !f.disk_ask.get_untracked() {
            f.disk_ask.set(true);
            return;
        }
        frozen.set(Some(s.clone()));
        card.with_value(|c| progress::start(f, c, &s));
    };

    view! {
        <article class="card mf-card" aria-label=card.with_value(|c| c.name.clone())>
            <header class="mf-card-head">
                <div class="mf-title">
                    <h2>{card.with_value(|c| c.name.clone())}</h2>
                    <span class="mf-by dim">{by}</span>
                </div>
                {head}
            </header>
            {can}
            <p class="mf-line">{ctx_line}</p>
            {uses}
            {other}
            {has_file.then_some(download)}
            {licence}
            <Show when=held>
                <p class="mf-line mf-warn">{words::HOLD_LINE}</p>
            </Show>
            <MachineInCard/>
            <Show when=move || shown.with(|s| s.fit.mark == FitMark::NoCard)>
                <no_card::AskMemory f=f/>
            </Show>
            {move || disk().map(|l| view! {
                <p class="mf-line mf-warn">
                    {l}
                    {move || f.disk_ask.get().then_some(". Press Get it anyway to download it all the same.")}
                </p>
            })}
            {move || metered().map(|why| {
                let check = check_exactly.clone();
                view! {
                    <div class="mf-line mf-metered">
                        <span title=why>"Your connection is metered, so lmgw didn't read the file to check it exactly."</span>
                        <button
                            class="btn sm"
                            disabled=move || f.marks_busy.get()
                            title="Reads the start of each file it needs, a few MiB each, mostly the model's vocabulary"
                            on:click=check
                        >
                            "Check exactly"
                        </button>
                        <span class="dim">"reads the start of the files it needs, a few MiB each"</span>
                    </div>
                }
            })}
            {move || f.marks_err.get().map(|e| view! {
                <div class="mf-line dim">
                    "lmgw couldn't read the file's header, so the marks stay by its size."
                    <super::Raw raw=e/>
                </div>
            })}
            {has_file.then_some(fold)}
            <PrivateLine card=card shown=shown/>
            {nothing_fits.then(|| view! {
                <p class="mf-line dim">"The file list shows every file of the upload with its fit."</p>
            })}
            {move || match f.get.get() {
                Getting::Idle | Getting::Failed(_) | Getting::Canceled => {
                    let (upload, _) = upload_target.clone();
                    view! {
                        {move || match f.get.get() {
                            Getting::Failed(e) => Some(view! { <div class="notice err mf-notice">{e}</div> }.into_any()),
                            Getting::Canceled => Some(view! { <p class="mf-line dim">"Cancelled. What was downloaded is kept for the next Get it."</p> }.into_any()),
                            _ => None,
                        }}
                        <footer class="mf-actions">
                            {(!upload.is_empty()).then(|| {
                                let u = upload.clone();
                                view! {
                                    <button class="btn" on:click=move |_| {
                                        let repo = shown.with_untracked(|s| s.upload.clone());
                                        open_list(if repo.is_empty() { u.clone() } else { repo });
                                    }>
                                        "Choose the file myself"
                                    </button>
                                }
                            })}
                            <SendToFriend f=f card=card shown=shown/>
                            <span class="mf-spacer"></span>
                            {has_file.then(|| view! {
                                <button
                                    class=move || if fits() { "btn primary mf-get" } else { "btn mf-get" }
                                    disabled=move || !can_get()
                                    title=move || (!fits()).then_some("It doesn't fit your graphics card: it would be refused when it loads")
                                    on:click=get_it
                                >
                                    {move || match (f.get.get(), f.disk_ask.get()) {
                                        (_, true) => "Get it anyway",
                                        (Getting::Failed(_) | Getting::Canceled, _) => "Get it again",
                                        _ => "Get it",
                                    }}
                                </button>
                            })}
                        </footer>
                    }
                        .into_any()
                }
                Getting::Starting => view! {
                    <footer class="mf-actions"><span class="dim">"Queueing the download…"</span></footer>
                }
                    .into_any(),
                Getting::Running(started) => {
                    let rate = card.with_value(|c| c.download.rate_bytes_per_sec);
                    view! { <progress::GetProgress f=f started=started rate=rate/> }.into_any()
                }
                Getting::Done(done) => {
                    let name = card.with_value(|c| c.name.clone());
                    view! { <progress::Ready f=f done=done kind=kind name=name/> }.into_any()
                }
            }}
        </article>
        <Modal open=f.picking title="Choose the file" fill=true>
            {move || {
                let repo = pick_repo.get();
                let (initial_file, target) = shown.with_untracked(|s| {
                    (
                        (s.upload == repo).then(|| s.file.clone()),
                        s.target.clone().unwrap_or_else(|| words::target_of(kind).to_string()),
                    )
                });
                view! {
                    <HfWizard
                        initial_repo=repo
                        initial_file=initial_file.unwrap_or_default()
                        initial_target=target
                        on_pick=on_pick
                    />
                }
            }}
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fold_leaves_the_hold_to_the_card_and_says_a_note_once() {
        let fit = FileFit {
            notes: vec![words::HOLD_LINE.to_string(), "admission is off".into()],
            ..FileFit::default()
        };
        let list = vec!["admission is off".to_string(), "estimated".to_string()];
        assert_eq!(
            fold_notes(&fit, &list),
            vec!["admission is off", "estimated"]
        );
    }
}
