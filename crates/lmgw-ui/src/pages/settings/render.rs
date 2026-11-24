//! Settings — rendering the table: a category as a detail page (its hero
//! and its groups, which flow into two columns in a pane over 1000 px), a
//! group as a boxed preference group, and one row per setting
//! ([`row::def_view`]).

use super::*;
use crate::widgets::{Ico, Icon};

/// One row per control.
mod row;

/// Scroll a field into the middle of the pane and flash it once.
pub(super) fn flash(id: &str) {
    flash_at(id, web_sys::ScrollLogicalPosition::Center);
}

/// Scroll an element to `block` of the pane and flash it once.
pub(super) fn flash_at(id: &str, block: web_sys::ScrollLogicalPosition) {
    let Some(el) = document().get_element_by_id(id) else {
        return;
    };
    flash_el(el, block, "flash");
}

/// The machine check's group (`#machine` is inside it): its top, heading
/// included, into view and a ring around it once (review L10). The ring is
/// a box-shadow that fades to none, never an opacity: WebKitGTK blanks an
/// element a frame after an opacity transition ends.
pub(super) fn flash_card(id: &str) {
    let Some(el) = document().get_element_by_id(id) else {
        return;
    };
    let card = el.closest(".pref-group").ok().flatten().unwrap_or(el);
    flash_el(card, web_sys::ScrollLogicalPosition::Start, "mc-flash");
}

fn flash_el(el: web_sys::Element, block: web_sys::ScrollLogicalPosition, class: &'static str) {
    let opts = web_sys::ScrollIntoViewOptions::new();
    opts.set_block(block);
    el.scroll_into_view_with_scroll_into_view_options(&opts);
    let _ = el.class_list().remove_1(class);
    // Re-adding the class in the same frame would not restart the animation.
    let again = el.clone();
    request_animation_frame(move || {
        let _ = again.class_list().add_1(class);
    });
    set_timeout(
        move || {
            let _ = el.class_list().remove_1(class);
        },
        Duration::from_millis(1800),
    );
}

/// Said under a field the machine check's settings switch changed after it
/// was edited (review H1).
pub(super) const SWITCHED_UNDER: &str = "The graphics card switch changed this setting after you \
                                         edited it. Saving writes your edit over the switch's \
                                         value.";

/// A `Select` or `ModelPicker` holds its own signal; this keeps one in step
/// with a form key both ways (Discard or a rebase moves the control, a pick
/// moves the draft).
pub(super) fn bridge(form: FormState, key: &'static str) -> RwSignal<String> {
    let now = move || {
        form.draft.with_untracked(|d| match d.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        })
    };
    let sig = RwSignal::new(now());
    Effect::new(move |_| {
        let t = form.text(key);
        if sig.get_untracked() != t {
            sig.set(t);
        }
    });
    Effect::new(move |_| {
        let v = sig.get();
        if now() != v {
            form.set_text(key, v);
        }
    });
    sig
}

/// A category's glyph, in the list and on its page.
pub(super) fn cat_icon(slug: &str) -> Ico {
    match slug {
        "network" => Ico::Globe,
        "retention" => Ico::Clock,
        "usage-cost" => Ico::Coins,
        "gpu" => Ico::Chip,
        "runtimes" => Ico::Box,
        "backends" => Ico::Wrench,
        "chat" => Ico::Chat,
        "documents" => Ico::File,
        "realtime" => Ico::Mic,
        "agents" => Ico::Bot,
        "docs" => Ico::Book,
        "tokens" => Ico::Key,
        "appearance" => Ico::Palette,
        _ => Ico::Info,
    }
}

/// A category's page: its glyph, name and what is in it, then its groups.
/// A search shows every category with a match this way, one under the
/// other, the name a link to the whole category.
pub(super) fn cat_view(
    c: &'static (&'static str, &'static str, &'static str),
    page: Page,
) -> impl IntoView {
    let (slug, label, blurb) = *c;
    let shown = move || {
        if page.searching() {
            fields().any(|d| d.cat == slug && page.matches(d))
        } else {
            page.active.get() == slug
        }
    };
    view! {
        <div class="detail-flow set-flow" data-cat=slug hidden=move || !shown()>
            <div class="detail-hero set-hero">
                <div class="glyph">
                    <Icon ico=cat_icon(slug)/>
                </div>
                <div class="hero-text">
                    <h1>
                        {move || {
                            if page.searching() {
                                view! { <a href=format!("/settings/{slug}")>{label}</a> }.into_any()
                            } else {
                                label.into_any()
                            }
                        }}
                    </h1>
                    <div class="sub">{super::prow::sentence(blurb)}</div>
                </div>
                // The one category with actions of its own in its hero:
                // Documents' check and its expert switch (design §8).
                {(slug == "documents").then(|| view! { <super::documents::HeroActions/> })}
            </div>
            // In two columns a group that spans them cuts the short ones in half,
            // so those come last, except one that opens the category (nothing is
            // above it to cut). One column puts every group back in the table's
            // order by CSS (`order`).
            {{
                let all: Vec<&'static Group> = GROUPS.iter().filter(|g| g.cat == slug).collect();
                let lead = all.iter().take_while(|g| g.wide).count();
                all[..lead]
                    .iter()
                    .copied()
                    .chain(all[lead..].iter().copied().filter(|g| !g.wide))
                    .chain(all[lead..].iter().copied().filter(|g| g.wide))
                    .map(|g| {
                        let at = all.iter().position(|o| o.id == g.id).unwrap_or(0) + 1;
                        group_view(g, at, page)
                    })
                    .collect_view()
            }}
        </div>
    }
}

/// A group: its heading (a fold's opens and closes it), its folded
/// explanation, and the box of its rows. A runtime class's group folds
/// (`lmgw.ui.open.<fold>`), and folded says what it runs and where from —
/// and that something in it is unsaved. A search opens every fold.
fn group_view(grp: &'static Group, at: usize, page: Page) -> AnyView {
    let defs: Vec<&'static Def> = fields().filter(|d| d.group == grp.id).collect();
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    // The Smart Turn cells are one table; every other setting is a row.
    let mut runs: Vec<(bool, Vec<&'static Def>)> = Vec::new();
    for d in &defs {
        let vad = matches!(d.ctl, Ctl::Vad(_));
        match runs.last_mut() {
            Some((true, v)) if vad => v.push(d),
            _ => runs.push((vad, vec![d])),
        }
    }
    let rows = runs
        .into_iter()
        .map(|(vad, v)| {
            if vad {
                let shown = v.clone();
                view! {
                    <div
                        class="prow set-row stack panel-row"
                        hidden=move || !shown.iter().any(|d| page.shows(d))
                    >
                        {realtime::vad_table(v, page)}
                    </div>
                }
                .into_any()
            } else {
                v.into_iter()
                    .map(|d| row::def_view(d, page))
                    .collect_view()
                    .into_any()
            }
        })
        .collect_view();
    let open =
        (!grp.fold.is_empty()).then(|| page.fold(grp.id).unwrap_or_else(|| RwSignal::new(true)));
    let is_open = move || page.searching() || open.is_none_or(|o| o.get());
    let form = page.form;
    let prefix = grp.id;
    let unsaved = move || {
        let n = form
            .dirty_keys()
            .iter()
            .filter(|k| k.starts_with(&format!("{prefix}.")))
            .count();
        (n > 0).then(|| format!("{n} unsaved"))
    };
    let what = move || {
        let mut parts = vec![
            form.text(&format!("{prefix}.image")),
            form.text(&format!("{prefix}.models_dir")),
        ];
        parts.retain(|p| !p.is_empty());
        parts.join(" · ")
    };
    // The whole line, for the tooltip of its ellipsis.
    let summary = move || match unsaved() {
        Some(u) if !what().is_empty() => format!("{u} · {}", what()),
        Some(u) => u,
        None => what(),
    };
    let head = match open {
        None => view! {
            <div class="pref-head">
                <h2>{grp.title}</h2>
            </div>
        }
        .into_any(),
        Some(o) => view! {
            <div class="pref-head">
                <button
                    type="button"
                    class="pref-fold"
                    class:open=is_open
                    aria-expanded=move || is_open().to_string()
                    disabled=move || page.searching()
                    on:click=move |_| o.update(|v| *v = !*v)
                >
                    <Icon ico=Ico::Right class="pref-fold-chev"/>
                    <h2>{grp.title}</h2>
                    <span class="pref-fold-sub">{grp.sub}</span>
                </button>
            </div>
            <p class="pref-desc set-fold-summary" hidden=is_open title=summary>
                {move || {
                    unsaved()
                        .map(|u| {
                            let sep = if what().is_empty() { "" } else { " · " };
                            view! {
                                <span class="set-fold-unsaved">{u}</span>
                                {sep}
                            }
                        })
                }}
                {what}
            </p>
        }
        .into_any(),
    };
    view! {
        <section
            class="pref-group set-group"
            class:wide=grp.wide
            data-group=grp.id
            style=format!("order: {at}")
            hidden=move || !any_shown()
        >
            {head}
            <div class="set-group-body" hidden=move || !is_open()>
                {explain(grp.id, page)}
                <div class="boxed set-box">{rows}</div>
            </div>
        </section>
    }
    .into_any()
}
