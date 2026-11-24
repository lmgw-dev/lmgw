//! Settings — one setting's row, by its control: numbers, text, switches
//! and choices at the row's end; paths, lists, prompts and pickers under
//! the title; the panels (the machine check, accounts, the listeners) as
//! one stacked row of their own; links and actions that act at once as
//! rows with a chevron or a button.

use super::super::prow::{
    choice_field, entry_field, lines_field, number_field, switch_field, SetField,
};
use super::*;

pub(super) fn def_view(d: &'static Def, page: Page) -> AnyView {
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
    // Set by the server's container (headless server design §2.2): every
    // control drawn here is shown locked, and the hint says where the value
    // is changed instead. The kinds drawn by a component of their own
    // (ForgeTokens, Words, Voice, the panels) hold no pinned setting yet.
    let pinned = Memo::new(move |_| page.pinned_why(k));
    let locked = Signal::derive(move || pinned.with(Option::is_some));
    let hint = Signal::derive(move || pinned.get().unwrap_or_else(|| d.hint.to_string()));
    // "restart to apply" only where a restart is the tray's (§6.2).
    let unit = Signal::derive(move || page.unit_of(d));
    match d.ctl {
        Ctl::Int(_) | Ctl::IntRange(..) | Ctl::OptIntRange(..) | Ctl::Float(..) | Ctl::Money => {
            view! {
                <SetField
                    label=d.label
                    hint=hint
                    inline=true
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    {number_field(form, d, move || page.unit_of(d), locked)}
                </SetField>
            }
            .into_any()
        }
        Ctl::Text | Ctl::Mono => {
            let mono = matches!(d.ctl, Ctl::Mono);
            // A path or a long address wants the row's width.
            let inline = d.size != Size::L;
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    inline=inline
                    class=if inline { "entry" } else { "" }
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    {entry_field(form, d, mono, locked)}
                </SetField>
            }
            .into_any()
        }
        Ctl::Bool => {
            let flip = Callback::new(move |()| {
                if !locked.get_untracked() {
                    form.set_flag(k, !form.flag(k));
                }
            });
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    inline=true
                    dirty=dirty
                    id=id
                    hidden=hidden
                    on_click=flip
                >
                    {switch_field(form, d, locked)}
                </SetField>
            }
            .into_any()
        }
        Ctl::Choice(opts) => {
            let options = Signal::derive(move || {
                opts.iter()
                    .map(|(v, l)| (v.to_string(), l.to_string()))
                    .collect::<Vec<_>>()
            });
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    inline=true
                    class="combo"
                    dirty=dirty
                    error=error
                    id=id
                    hidden=hidden
                >
                    {choice_field(form, d, options, None, locked)}
                </SetField>
            }
            .into_any()
        }
        Ctl::Profile => view! {
            <SetField
                label=d.label
                unit=unit
                hint=hint
                inline=true
                class="combo"
                dirty=dirty
                id=id
                hidden=hidden
            >
                {choice_field(form, d, chat_profile::options(), Some("Default"), locked)}
            </SetField>
        }
        .into_any(),
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
            // matching it) and its class group open — not on every visit.
            let visible = Signal::derive(move || {
                page.shows(d) && (page.searching() || page.fold(d.group).is_none_or(|o| o.get()))
            });
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    class="field-row"
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <fieldset class="set-lock" disabled=locked>
                        <ImagePicker value=value class=class backend=backend visible=visible/>
                    </fieldset>
                </SetField>
            }
            .into_any()
        }
        Ctl::ForgeTokens => view! {
            <ForgeTokensField page=page id=id hidden=hidden error=error warn=warn/>
        }
        .into_any(),
        Ctl::BuildsDir => {
            let server = move |key: &'static str| {
                page.data
                    .with(|d| d.as_ref().and_then(|v| v[key].as_str()).map(str::to_string))
            };
            let effective = move || server("builds_dir_effective").unwrap_or_default();
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    hint_extra=ViewFn::from(move || {
                        view! { "· in use now: " <code>{effective}</code> }
                    })
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    <input
                        class="prow-entry mono"
                        placeholder=effective
                        spellcheck="false"
                        aria-label=d.label
                        disabled=locked
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    />
                    {move || {
                        server("builds_dir_warning")
                            .map(|w| view! { <div class="notice warn set-notice">{w}</div> })
                    }}
                </SetField>
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
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    class="field-row"
                    dirty=dirty
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    <fieldset class="set-lock" disabled=locked>
                        {picker}
                    </fieldset>
                </SetField>
            }
            .into_any()
        }
        Ctl::Secret(has, clear) => {
            let stored = move || page.server_flag(has);
            let either = Signal::derive(move || form.is_dirty(k) || form.is_dirty(clear));
            view! {
                <SetField
                    label=d.label
                    unit=Signal::derive(move || if stored() { "stored" } else { "not set" })
                    hint=hint
                    inline=true
                    class="entry"
                    dirty=either
                    error=Signal::derive(move || form.error(k))
                    id=id
                    hidden=hidden
                >
                    <input
                        class="prow-entry mono"
                        type="password"
                        autocomplete="off"
                        aria-label=d.label
                        disabled=locked
                        placeholder=move || if stored() { "Type to replace" } else { "" }
                        prop:value=move || form.text(k)
                        on:input=move |ev| form.set_text(k, event_target_value(&ev))
                    />
                    <Show when=stored>
                        <label class="check set-clear" title="Erased on Save">
                            <input
                                type="checkbox"
                                disabled=locked
                                prop:checked=move || form.flag(clear)
                                on:change=move |ev| form.set_flag(clear, event_target_checked(&ev))
                            />
                            "Clear"
                        </label>
                    </Show>
                </SetField>
            }
            .into_any()
        }
        Ctl::Lines | Ctl::Map => {
            let warn = if matches!(d.ctl, Ctl::Map) {
                warn
            } else {
                Signal::stored(None)
            };
            view! {
                <SetField
                    label=d.label
                    unit=unit
                    hint=hint
                    dirty=dirty
                    error=error
                    warn=warn
                    id=id
                    hidden=hidden
                >
                    {lines_field(form, d, true, locked)}
                </SetField>
            }
            .into_any()
        }
        Ctl::Prompt(builtin_at) => prompt_row(page, d, id, hidden, dirty, hint, locked, builtin_at),
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
        Ctl::TagHint => {
            view! { <TagHintField d=d page=page dirty=dirty id=id hidden=hidden/> }.into_any()
        }
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
        Ctl::Budget => panel(
            id,
            hidden,
            view! { <BudgetPanel d=d page=page id=String::new() hidden=hidden/> },
        ),
        // Drawn as one table by `group_view`; never reached.
        Ctl::Vad(_) => ().into_any(),
        Ctl::Link(href) => link_row(page, d, id, hidden, href),
        Ctl::DeviceListener => panel(
            id,
            hidden,
            view! { <devices::ListenerPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::WebRtc => panel(
            id,
            hidden,
            view! { <webrtc::WebRtcPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::TlsStatus => panel(
            id,
            hidden,
            view! { <tls::TlsPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::Accounts => panel(
            id,
            hidden,
            view! { <accounts::AccountsPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::AuditLog => panel(
            id,
            hidden,
            view! { <accounts::AuditPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::About => panel(
            id,
            hidden,
            view! { <about::AboutPanel page=page id=String::new() hidden=hidden/> },
        ),
        Ctl::Documents(part) => view! {
            <documents::DocPanel part=part id=id hidden=hidden/>
        }
        .into_any(),
        Ctl::Said(pointer) => {
            let said = move || {
                page.data.with(|d| {
                    d.as_ref()
                        .and_then(|v| v.pointer(pointer))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_default()
                })
            };
            view! {
                <div class="prow set-row notice-row" id=id hidden=hidden>
                    {said}
                </div>
            }
            .into_any()
        }
        Ctl::HeadroomCards => {
            view! { <headroom::HeadroomCards page=page id=id hidden=hidden/> }.into_any()
        }
        Ctl::Hold => view! { <HoldRow page=page id=id hidden=hidden/> }.into_any(),
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
                <div
                    class="prow set-row stack panel-row set-machine"
                    id=MACHINE_ANCHOR
                    hidden=move || hidden.get()
                >
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
                <a
                    class="prow set-row action"
                    href=format!("/settings/gpu#{MACHINE_ANCHOR}")
                    id=id
                    hidden=move || hidden.get()
                >
                    <div class="prow-text">
                        <span class="prow-title">
                            {move || crate::widgets::machine_check::card_heading(page.container())}
                        </span>
                        <span class="prow-sub">{said}</span>
                    </div>
                    <Icon ico=Ico::Right class="prow-chevron"/>
                </a>
            }
            .into_any()
        }
        Ctl::Apply(target) => {
            // `apply.router` → the chat class's own keys, `router.*`.
            let prefix = format!("{}.", k.trim_start_matches("apply."));
            let unsaved =
                Signal::derive(move || form.dirty_keys().iter().any(|d| d.starts_with(&prefix)));
            view! {
                <SetField
                    label=d.label
                    unit="applies now"
                    hint="Recreates every running model of this class with the saved settings"
                    inline=true
                    id=id
                    hidden=hidden
                >
                    <ClassApplyButton target=target unsaved=unsaved/>
                </SetField>
            }
            .into_any()
        }
        Ctl::CheckNow => view! {
            <SetField
                label="App update"
                unit="applies now"
                hint="Asks now whether a newer lmgw is out"
                inline=true
                id=id
                hidden=hidden
            >
                <CheckNowButton/>
            </SetField>
        }
        .into_any(),
        Ctl::Theme => view! { <ThemeField id=id hidden=hidden/> }.into_any(),
        Ctl::Scale => view! { <ScaleField id=id hidden=hidden/> }.into_any(),
    }
}

/// A read panel (a listener's state, accounts, the audit log) as one row of
/// its own across the box: the anchor and the search's hiding are the
/// row's, the panel keeps its own layout inside.
fn panel(id: String, hidden: Signal<bool>, inner: impl IntoView + 'static) -> AnyView {
    view! {
        <div class="prow set-row stack panel-row" id=id hidden=move || hidden.get()>
            {inner}
        </div>
    }
    .into_any()
}

/// A pointer to where the thing is really managed, as a row that leads
/// there.
fn link_row(
    page: Page,
    d: &'static Def,
    id: String,
    hidden: Signal<bool>,
    href: &'static str,
) -> AnyView {
    let what = move || match d.key {
        "api_keys" => {
            let n = page.data.with(|d| {
                d.as_ref()
                    .and_then(|v| v["api_keys"].as_array().map(Vec::len))
                    .unwrap_or(0)
            });
            format!("{n} keys, created, revoked, scoped and budgeted on Usage → Keys")
        }
        "backends_page" => {
            "The builds, their runs and every local image of the three engines are on Engines"
                .to_string()
        }
        _ => "Set on the Docs playground with its Save as defaults, next to the search that \
              measures them"
            .to_string(),
    };
    view! {
        <a class="prow set-row action" href=href id=id hidden=move || hidden.get()>
            <div class="prow-text">
                <span class="prow-title">{d.label}</span>
                <span class="prow-sub">{what}</span>
            </div>
            <Icon ico=Ico::Right class="prow-chevron"/>
        </a>
    }
    .into_any()
}

/// A default prompt: prose in a tall box under the title, and a Reset that
/// puts the built-in text back — the server's `…_builtin` at `builtin_at`.
#[allow(clippy::too_many_arguments)]
fn prompt_row(
    page: Page,
    d: &'static Def,
    id: String,
    hidden: Signal<bool>,
    dirty: Signal<bool>,
    hint: Signal<String>,
    locked: Signal<bool>,
    builtin_at: &'static str,
) -> AnyView {
    let form = page.form;
    let k = d.key;
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
        <SetField
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
            hint=hint
            dirty=dirty
            id=id
            hidden=hidden
        >
            <textarea
                class="prow-area set-area set-prompt"
                class:set-prompt-wide=placeholders
                rows=if placeholders { "18" } else { "6" }
                aria-label=d.label
                disabled=locked
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
                    class="btn sm"
                    disabled=move || is_builtin() || locked.get()
                    title="Put the built-in prompt back in the box; Save to keep it"
                    on:click=move |_| form.set_text(k, builtin())
                >
                    "Reset to built-in"
                </button>
            </div>
        </SetField>
    }
    .into_any()
}
