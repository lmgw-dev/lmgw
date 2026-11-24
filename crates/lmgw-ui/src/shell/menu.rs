//! The main menu in the sidebar's header: the theme and, in the app window,
//! the interface scale.

use leptos::prelude::*;
use leptos_router::components::A;

use crate::widgets::{Ico, Icon};

/// The main menu: the theme and, in the app window, the interface scale.
#[component]
pub(super) fn MainMenu(open: RwSignal<bool>) -> impl IntoView {
    let theme = crate::theme::use_theme();
    let scale = crate::ui_scale::use_ui_scale();
    let zoom = crate::ui_scale::supported();
    let locked = crate::session::locked_signal();
    let pick = move |v: String| {
        theme.choice.set(v);
        open.set(false);
    };
    view! {
        <div class="pop-inner menu-list" role="menu">
            <div class="menu-head">"Theme"</div>
            {theme
                .options()
                .into_iter()
                .map(|(v, label)| {
                    let on = {
                        let v = v.clone();
                        move || theme.choice.with(|c| *c == v)
                    };
                    view! {
                        <button
                            type="button"
                            class="menu-item"
                            class:checked=on.clone()
                            role="menuitemradio"
                            aria-checked=move || on().to_string()
                            on:click=move |_| pick(v.clone())
                        >
                            <span class="menu-check"></span>
                            {label}
                        </button>
                    }
                })
                .collect_view()}
            {zoom
                .then(|| {
                    view! {
                        <hr class="menu-sep"/>
                        <div class="menu-scale">
                            <span class="menu-scale-label">"Interface scale"</span>
                            <button
                                type="button"
                                class="icon-btn sm"
                                title="Smaller (Ctrl −)"
                                on:click=move |_| scale.nudge(-1)
                            >
                                <Icon ico=Ico::Minus/>
                            </button>
                            <button
                                type="button"
                                class="btn flat sm menu-scale-pct"
                                title=format!("Reset to {} % (Ctrl 0)", crate::ui_scale::DEFAULT)
                                on:click=move |_| scale.set(crate::ui_scale::DEFAULT)
                            >
                                {move || format!("{} %", scale.pct.get())}
                            </button>
                            <button
                                type="button"
                                class="icon-btn sm"
                                title="Larger (Ctrl +)"
                                on:click=move |_| scale.nudge(1)
                            >
                                <Icon ico=Ico::Plus/>
                            </button>
                        </div>
                    }
                })}
            <hr class="menu-sep"/>
            {move || {
                if locked.get() {
                    view! {
                        <span class="menu-item disabled" aria-disabled="true" title="Sign in first">
                            <span class="menu-check"></span>
                            "Settings"
                        </span>
                    }
                        .into_any()
                } else {
                    view! {
                        <span on:click=move |_| open.set(false)>
                            <A href="/settings" attr:class="menu-item">
                                <span class="menu-check"></span>
                                "Settings"
                            </A>
                        </span>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
