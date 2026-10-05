//! The echo chip (chat-voice §12.1, §12.2, §9.4): the window's echo mode,
//! amber with §12.2's warning when the input cancels echo against the
//! default output while lmgw plays elsewhere, and the four modes to choose
//! from. Used in the devices popover and the realtime panel.

use leptos::prelude::*;

use super::super::audio::devices::EchoMode;
use super::super::audio::shell;
use super::VoiceDevices;

/// `expanded`: the modes always listed, without the chip that opens them
/// (the realtime panel's echo popover, whose own chip opens it).
#[component]
pub(crate) fn EchoChip(dev: VoiceDevices, #[prop(optional)] expanded: bool) -> impl IntoView {
    let in_app = shell::in_shell();
    let open = RwSignal::new(expanded);
    let warning = Memo::new(move |_| dev.warning());
    view! {
        <div class="echo" data-echo=move || dev.echo.get().key()
            data-echo-warning=move || warning.get().map(|_| "yes").unwrap_or("no")>
            {(!expanded).then(|| view! {
                <button
                    type="button"
                    class="chip echo-chip"
                    class:info=move || warning.get().is_none()
                    class:warn=move || warning.get().is_some()
                    aria-expanded=move || open.get().to_string()
                    title="How lmgw keeps its own voice out of the microphone"
                    on:click=move |_| open.update(|o| *o = !*o)
                >
                    {move || format!("Echo: {}", dev.echo.get().label(in_app))}
                    <span class="select-arrow">"▾"</span>
                </button>
            })}
            {move || {
                warning
                    .get()
                    .map(|w| {
                        view! {
                            <div class="echo-warn">
                                <span>{w}</span>
                                <button
                                    type="button"
                                    class="link-btn"
                                    title="Ignore the microphone while lmgw speaks"
                                    on:click=move |_| dev.set_echo(EchoMode::None)
                                >
                                    "Use half duplex"
                                </button>
                            </div>
                        }
                    })
            }}
            <Show when=move || open.get()>
                <div class="echo-modes" role="radiogroup" aria-label="Echo mode">
                    {EchoMode::ALL
                        .into_iter()
                        .map(|m| {
                            view! {
                                <button
                                    type="button"
                                    role="radio"
                                    class="echo-mode"
                                    class:sel=move || dev.echo.get() == m
                                    aria-checked=move || (dev.echo.get() == m).to_string()
                                    data-mode=m.key()
                                    on:click=move |_| {
                                        dev.set_echo(m);
                                        if !expanded {
                                            open.set(false);
                                        }
                                    }
                                >
                                    <span class="echo-mode-name">
                                        {m.label(in_app)}
                                        {(m == EchoMode::Device).then_some(" (default)")}
                                    </span>
                                    <span class="echo-mode-hint">{m.hint()}</span>
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            </Show>
        </div>
    }
}
