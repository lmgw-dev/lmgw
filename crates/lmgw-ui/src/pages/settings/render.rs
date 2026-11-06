//! Settings — rendering the table: categories, groups, runs and one row per control.

use super::*;

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

/// The machine check's card (`#machine` is inside it): its top, heading
/// included, into view and a ring around it once (review L10). The ring is
/// a box-shadow that fades to none, never an opacity: WebKitGTK blanks an
/// element a frame after an opacity transition ends.
pub(super) fn flash_card(id: &str) {
    let Some(el) = document().get_element_by_id(id) else {
        return;
    };
    let card = el.closest(".card").ok().flatten().unwrap_or(el);
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
        <section class="set-cat" hidden=move || !shown()>
            <header class="set-cat-head">
                <h2>
                    <a href=format!("/settings/{slug}")>{label}</a>
                </h2>
                <span class="dim">{blurb}</span>
            </header>
            <div class="card-flow set-flow">
                {GROUPS
                    .iter()
                    .filter(|g| g.cat == slug)
                    .map(|g| group_view(g, page))
                    .collect_view()}
            </div>
        </section>
    }
}

/// How a run of fields is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lay {
    Grid(Size),
    Checks,
    Block,
    Acts,
    /// The Smart Turn table: a run of [`Ctl::Vad`] cells, one table.
    Vad,
}

fn lay(d: &Def) -> Lay {
    match d.ctl {
        Ctl::Bool => Lay::Checks,
        Ctl::Lines
        | Ctl::Link(_)
        | Ctl::Hold
        | Ctl::Machine
        | Ctl::MachineLink
        | Ctl::ForgeTokens
        | Ctl::Prompt(_)
        | Ctl::Map
        | Ctl::Words
        | Ctl::SpeechStyle(_)
        | Ctl::TagHint
        | Ctl::Budget
        | Ctl::DeviceListener
        | Ctl::WebRtc => Lay::Block,
        Ctl::Vad(_) => Lay::Vad,
        Ctl::Apply(_) | Ctl::CheckNow => Lay::Acts,
        Ctl::Theme | Ctl::Scale => Lay::Grid(Size::S),
        _ => Lay::Grid(d.size),
    }
}

fn group_view(grp: &'static Group, page: Page) -> AnyView {
    let defs: Vec<&'static Def> = fields().filter(|d| d.group == grp.id).collect();
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    let body = {
        let defs = defs.clone();
        move || {
            // Runs of one layout share a container: a field grid of one cell
            // size, a row of ticks, a block, the applies-now actions.
            let mut runs: Vec<(Lay, Vec<&'static Def>)> = Vec::new();
            for d in &defs {
                match runs.last_mut() {
                    Some((l, v)) if *l == lay(d) => v.push(d),
                    _ => runs.push((lay(d), vec![d])),
                }
            }
            view! {
                {runs.into_iter().map(|(l, v)| run_view(l, v, page)).collect_view()}
                {explain(grp.id, page)}
            }
        }
    };
    if grp.fold.is_empty() {
        let class = if grp.wide {
            "set-flow-item wide"
        } else {
            "set-flow-item"
        };
        return view! {
            <div class=class hidden=move || !any_shown()>
                <section class="card edit-section set-group">
                    <h3>{grp.title}</h3>
                    {body()}
                </section>
            </div>
        }
        .into_any();
    }
    let form = page.form;
    let prefix = grp.id;
    // Folded, the section still says what it runs and where from — and that
    // something in it is unsaved.
    let summary = Signal::derive(move || {
        let n = form
            .dirty_keys()
            .iter()
            .filter(|k| k.starts_with(&format!("{prefix}.")))
            .count();
        let mut parts = Vec::new();
        if n > 0 {
            parts.push(format!("{n} unsaved"));
        }
        parts.push(form.text(&format!("{prefix}.image")));
        parts.push(form.text(&format!("{prefix}.models_dir")));
        parts.retain(|p| !p.is_empty());
        parts.join(" · ")
    });
    let open = page.fold(grp.id).unwrap_or_else(|| RwSignal::new(true));
    let title = format!("{} · {}", grp.title, grp.sub);
    view! {
        <div class="set-flow-item" hidden=move || !any_shown()>
            <div class="card set-group set-fold">
                <Section
                    title=title
                    summary=summary
                    open=open
                    force_open=Signal::derive(move || page.searching())
                >
                    {body()}
                </Section>
            </div>
        </div>
    }
    .into_any()
}

fn run_view(l: Lay, defs: Vec<&'static Def>, page: Page) -> AnyView {
    if l == Lay::Vad {
        return realtime::vad_table(defs, page);
    }
    let any_shown = {
        let defs = defs.clone();
        move || defs.iter().any(|d| page.shows(d))
    };
    let items = defs.into_iter().map(|d| def_view(d, page)).collect_view();
    let class = match l {
        Lay::Grid(Size::S) => "field-grid fg-s",
        Lay::Grid(Size::M) => "field-grid fg-m",
        Lay::Grid(Size::L) => "field-grid fg-l",
        Lay::Checks => "set-checks",
        Lay::Block => "set-blocks",
        Lay::Acts => "set-acts",
        Lay::Vad => unreachable!("drawn by realtime::vad_table above"),
    };
    view! { <div class=class hidden=move || !any_shown()>{items}</div> }.into_any()
}

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

fn def_view(d: &'static Def, page: Page) -> AnyView {
    let form = page.form;
    let k = d.key;
    let id = anchor(k);
    let hidden = Signal::derive(move || !page.shows(d));
    let dirty = form.dirty_signal(k);
    // A problem of a field this save does not judge is the stored value's:
    // a warning, and Save goes ahead (`judged_keys`).
    let shown = Memo::new(move |_| messages(form, d, page.judged.with(|j| j.contains(k))));
    let error = Signal::derive(move || form.error(k).or_else(|| shown.get().0));
    let warn = Signal::derive(move || {
        shown.get().1.or_else(|| {
            (page.under.with(|u| u.contains(k)) && form.is_dirty(k))
                .then(|| SWITCHED_UNDER.to_string())
        })
    });
    match d.ctl {
        Ctl::Text
        | Ctl::Mono
        | Ctl::Int(_)
        | Ctl::IntRange(..)
        | Ctl::OptIntRange(..)
        | Ctl::Float(..)
        | Ctl::Money => {
            let money = matches!(d.ctl, Ctl::Money);
            let numeric = matches!(
                d.ctl,
                Ctl::Int(_) | Ctl::IntRange(..) | Ctl::OptIntRange(..) | Ctl::Float(..) | Ctl::Money
            );
            let class = if matches!(d.ctl, Ctl::Text) {
                "input"
            } else {
                "input mono"
            };
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <input
                        class=class
                        placeholder=d.ph
                        inputmode=if numeric { "decimal" } else { "text" }
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| {
                            let v = event_target_value(&ev);
                            // Comma decimals are how half the world writes
                            // money; "10,50" means 10.50, not "no budget".
                            form.set_text(k, if money { v.replace(',', ".") } else { v });
                        }
                    />
                </Field>
            }
            .into_any()
        }
        Ctl::Choice(opts) => {
            let value = bridge(form, k);
            let options = Signal::derive(move || {
                opts.iter()
                    .map(|(v, l)| (v.to_string(), l.to_string()))
                    .collect::<Vec<_>>()
            });
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <Select value=value options=options/>
                </Field>
            }
            .into_any()
        }
        Ctl::Profile => {
            let value = bridge(form, k);
            let options = chat_profile::options();
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <Select value=value options=options placeholder="Default"/>
                </Field>
            }
            .into_any()
        }
        Ctl::Image(class) => {
            let value = bridge(form, k);
            // The audio class says which GPU backend it runs; an image built
            // for another one is worth a warning.
            let backend = Signal::derive(move || {
                if class == ImageClass::Audio {
                    form.text("audio.backend")
                } else {
                    String::new()
                }
            });
            // Every category stays mounted: the picker reads the local
            // images once it is on screen — its category shown (or a search
            // matching it) and its class section open — not on every visit.
            let visible = Signal::derive(move || {
                page.shows(d)
                    && (page.searching() || page.fold(d.group).is_none_or(|o| o.get()))
            });
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <ImagePicker value=value class=class backend=backend visible=visible/>
                </Field>
            }
            .into_any()
        }
        Ctl::ForgeTokens => view! {
            <ForgeTokensField page=page id=id hidden=hidden error=error warn=warn/>
        }
        .into_any(),
        Ctl::BuildsDir => {
            let server = move |key: &'static str| {
                page.data.with(|d| {
                    d.as_ref()
                        .and_then(|v| v[key].as_str())
                        .map(str::to_string)
                })
            };
            let effective = move || server("builds_dir_effective").unwrap_or_default();
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <input
                        class="input mono"
                        placeholder=effective
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    />
                    <div class="field-hint">
                        "in use now: " <code>{effective}</code>
                    </div>
                    {move || {
                        server("builds_dir_warning")
                            .map(|w| view! { <div class="notice warn set-notice">{w}</div> })
                    }}
                </Field>
            }
            .into_any()
        }
        Ctl::Model(tasks, no_local, empty) => {
            let value = bridge(form, k);
            let picker = match no_local {
                Some(reason) => view! {
                    <ModelPicker
                        value=value
                        tasks=tasks
                        empty_label=empty
                        recent_key="settings"
                        disallow=(Callback::new(|e: CatalogEntry| e.local), reason)
                    />
                }
                .into_any(),
                None => view! {
                    <ModelPicker value=value tasks=tasks empty_label=empty recent_key="settings"/>
                }
                .into_any(),
            };
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    {picker}
                </Field>
            }
            .into_any()
        }
        Ctl::Secret(has, clear) => {
            let stored = move || page.server_flag(has);
            let either = Signal::derive(move || form.is_dirty(k) || form.is_dirty(clear));
            view! {
                <Field
                    label=d.label
                    unit=Signal::derive(move || if stored() { "stored" } else { "not set" })
                    hint=d.hint
                    dirty=either
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    <div class="row secret-row">
                        <input
                            class="input mono"
                            type="password"
                            autocomplete="off"
                            placeholder=move || if stored() { "type to replace" } else { "" }
                            prop:value=move || form.text(k)
                            on:input=move |ev| form.set_text(k, event_target_value(&ev))
                        />
                        <Show when=stored>
                            <label class="check" title="Erased on Save">
                                <input
                                    type="checkbox"
                                    prop:checked=move || form.flag(clear)
                                    on:change=move |ev| form.set_flag(clear, event_target_checked(&ev))
                                />
                                "clear"
                            </label>
                        </Show>
                    </div>
                </Field>
            }
            .into_any()
        }
        Ctl::Bool => view! {
            <label class="check check-line" class:dirty=move || dirty.get() id=id hidden=move || hidden.get()>
                <input
                    type="checkbox"
                    prop:checked=move || form.flag(k)
                    on:change=move |ev| form.set_flag(k, event_target_checked(&ev))
                />
                <span>{d.label}</span>
                {(!d.hint.is_empty()).then(|| view! { <span class="check-hint">{d.hint}</span> })}
            </label>
        }
        .into_any(),
        Ctl::Lines => {
            // As many rows as there are lines, and one to type into.
            let rows = move || (form.text(k).lines().count() + 1).max(2).to_string();
            view! {
                <Field label=d.label unit=d.unit hint=d.hint dirty=dirty id=id hidden=hidden>
                    <textarea
                        class="input mono ta set-ta"
                        rows=rows
                        spellcheck="false"
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                </Field>
            }
            .into_any()
        }
        Ctl::Prompt(builtin_at) => {
            let builtin = move || {
                page.data.with(|d| {
                    d.as_ref()
                        .and_then(|v| v.pointer(builtin_at))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_default()
                })
            };
            // What Save would store is the built-in default when the box holds
            // its text: that one keeps following it as it improves.
            let is_builtin = move || form.text(k).trim() == builtin().trim();
            // Only the Chat's prompt has placeholders; the voice prompt goes
            // to the model as it is.
            let placeholders = k == "chat_system_prompt";
            view! {
                <Field
                    label=d.label
                    unit=Signal::derive(move || {
                        if is_builtin() {
                            "built-in"
                        } else if form.text(k).trim().is_empty() {
                            "none"
                        } else {
                            "your own"
                        }
                    })
                    hint=d.hint
                    dirty=dirty
                    id=id
                    hidden=hidden
                >
                    <textarea
                        class="input ta set-ta set-prompt"
                        rows=if placeholders { "18" } else { "6" }
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                    <div class="set-prompt-foot">
                        {if placeholders {
                            view! {
                                <span class="field-hint">
                                    <code>"{{model}}"</code>
                                    " becomes the thread's model alias and "
                                    <code>"{{date}}"</code>
                                    " today's date, each time a message is sent."
                                </span>
                            }
                                .into_any()
                        } else {
                            view! {
                                <span class="field-hint">
                                    "Reset follows the built-in text as releases improve it; an \
                                     empty box sends none."
                                </span>
                            }
                                .into_any()
                        }}
                        <button
                            class="btn ghost sm"
                            disabled=is_builtin
                            title="Put the built-in prompt back in the box; Save to keep it"
                            on:click=move |_| form.set_text(k, builtin())
                        >
                            "Reset to built-in"
                        </button>
                    </div>
                </Field>
            }
            .into_any()
        }
        Ctl::Map => {
            let rows = move || (form.text(k).lines().count() + 1).max(2).to_string();
            view! {
                <Field
                    label=d.label
                    unit=d.unit
                    hint=d.hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <textarea
                        class="input mono ta set-ta"
                        rows=rows
                        spellcheck="false"
                        placeholder=d.ph
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    ></textarea>
                </Field>
            }
            .into_any()
        }
        Ctl::Words => view! {
            <WordsField d=d page=page dirty=dirty error=error id=id hidden=hidden/>
        }
        .into_any(),
        Ctl::Voice(keys) => view! {
            <VoiceField d=d page=page dirty=dirty error=error id=id hidden=hidden tts_keys=keys/>
        }
        .into_any(),
        Ctl::SpeechStyle(keys) => view! {
            <SpeechStyleField
                d=d
                page=page
                dirty=dirty
                error=error
                id=id
                hidden=hidden
                tts_keys=keys
            />
        }
        .into_any(),
        Ctl::TagHint => view! { <TagHintField d=d page=page dirty=dirty id=id hidden=hidden/> }
            .into_any(),
        Ctl::MediaModel(field) => view! {
            <chat_media::MediaDefaultField
                d=d
                page=page
                field=field
                dirty=dirty
                error=error
                id=id
                hidden=hidden
            />
        }
        .into_any(),
        Ctl::MediaVoice => view! {
            <chat_media::MediaDefaultField
                d=d
                page=page
                field=lmgw_api_types::chat_media::MediaField::Voice
                dirty=dirty
                error=error
                id=id
                hidden=hidden
            />
        }
        .into_any(),
        Ctl::VoiceLanguage => view! {
            <chat_voice::LanguageField
                d=d
                page=page
                dirty=dirty
                error=error
                warn=warn
                id=id
                hidden=hidden
            />
        }
        .into_any(),
        Ctl::Budget => view! { <BudgetPanel d=d page=page id=id hidden=hidden/> }
            .into_any(),
        // Drawn as one table by `run_view`; never reached.
        Ctl::Vad(_) => ().into_any(),
        Ctl::Link(href) => {
            let what = move || match k {
                "api_keys" => {
                    let n = page.data.with(|d| {
                        d.as_ref()
                            .and_then(|v| v["api_keys"].as_array().map(Vec::len))
                            .unwrap_or(0)
                    });
                    format!("{n} keys · created, revoked, scoped and budgeted on")
                }
                "backends_page" => {
                    "the builds, their runs and every local image of the three engines are on"
                        .to_string()
                }
                _ => "set on the Docs playground with its Save as defaults, next to the search \
                      that measures them —"
                    .to_string(),
            };
            let target = match k {
                "api_keys" => "Usage → Keys",
                "backends_page" => "Engines",
                _ => "Docs → Playground",
            };
            view! {
                <div class="set-line" id=id hidden=move || hidden.get()>
                    <span class="set-line-label">{d.label}</span>
                    <span class="dim">{what}</span>
                    <a href=href>{target}</a>
                </div>
            }
            .into_any()
        }
        Ctl::DeviceListener => {
            view! { <devices::ListenerPanel page=page id=id hidden=hidden/> }.into_any()
        }
        Ctl::WebRtc => view! { <webrtc::WebRtcPanel page=page id=id hidden=hidden/> }.into_any(),
        Ctl::Hold => view! {
            <div class="set-line" id=id hidden=move || hidden.get()>
                <HoldSwitch page=page/>
            </div>
        }
        .into_any(),
        // The card's anchor is `#machine`, the link every other surface
        // (the tray, WP8b's cards) sends people to (§2.3). It asks for a
        // fresh run when it is first shown (§2.6), so it mounts only once
        // the GPU category is open: every category stays mounted, and the
        // Network category is no reason to probe the card.
        //
        // `#machine` scrolls the card in once it has its content (the
        // check, or why there is none), not on a timer: the content comes
        // from a read, after the settings did (review L10).
        Ctl::Machine => {
            let seen = RwSignal::new(false);
            Effect::new(move |_| {
                if !hidden.get() && !seen.get_untracked() {
                    seen.set(true);
                }
            });
            let st = crate::widgets::machine_check::use_machine_state();
            Effect::new(move |_| {
                if !page.machine_scroll.get() || hidden.get() || !seen.get() {
                    return;
                }
                if st.shown_or_failed() {
                    page.machine_scroll.set(false);
                    request_animation_frame(|| flash_card(MACHINE_ANCHOR));
                }
            });
            view! {
                <div class="set-machine" id=MACHINE_ANCHOR hidden=move || hidden.get()>
                    <Show when=move || seen.get()>
                        <MachineCheck fresh=true settings_written=page.settings_written/>
                    </Show>
                </div>
            }
            .into_any()
        }
        // One link per search, for every machine row it matches (review
        // L11), the matched names in front.
        Ctl::MachineLink => {
            let said = move || page.words.with(|w| link_words(&link_labels(w)));
            view! {
                <div class="set-line" id=id hidden=move || hidden.get()>
                    <span class="set-line-label">{said}</span>
                    <a href=format!("/settings/gpu#{MACHINE_ANCHOR}")>"This computer"</a>
                </div>
            }
            .into_any()
        }
        Ctl::Apply(target) => {
            // `apply.router` → the chat class's own keys, `router.*`.
            let prefix = format!("{}.", k.trim_start_matches("apply."));
            let unsaved =
                Signal::derive(move || form.dirty_keys().iter().any(|d| d.starts_with(&prefix)));
            view! {
                <span class="set-act" id=id hidden=move || hidden.get()>
                    <ClassApplyButton target=target unsaved=unsaved/>
                    <span class="applies-now">"applies now"</span>
                </span>
            }
            .into_any()
        }
        Ctl::CheckNow => view! {
            <span class="set-act" id=id hidden=move || hidden.get()>
                <CheckNowButton/>
                <span class="applies-now">"applies now"</span>
            </span>
        }
        .into_any(),
        Ctl::Theme => view! { <ThemeField id=id hidden=hidden/> }.into_any(),
        Ctl::Scale => view! { <ScaleField id=id hidden=hidden/> }.into_any(),
    }
}
