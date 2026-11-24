//! The kebab row menu (UX plan §4): a row's secondary actions in a popover,
//! a destructive one asking in place. Shared by the dashboard's tables and
//! the Chat's thread rows (`lmgw-chat-ui`).

use leptos::html;
use leptos::prelude::*;

use super::confirm::is_second_press;
use super::popover::{self, Popover};

/// One entry of a [`RowMenu`].
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub on_select: Callback<()>,
    /// Destructive: the first pick only arms it ("Delete? ✓"), the second
    /// runs it (UX plan §4: nothing destructive is one click).
    pub danger: bool,
    /// Red like `danger`, but the pick runs at once: for an item that opens
    /// its own confirmation.
    pub destructive: bool,
    pub disabled: bool,
    /// Why it is disabled, or what it does.
    pub title: Option<String>,
    /// A rule above it: it starts a group of its own.
    pub separated: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, on_select: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            label: label.into(),
            on_select: Callback::new(move |()| on_select()),
            danger: false,
            destructive: false,
            disabled: false,
            title: None,
            separated: false,
        }
    }

    /// A rule above it: it starts a group of its own. The first item of a
    /// menu has no rule above it.
    pub fn separated(mut self) -> Self {
        self.separated = true;
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Red like [`Self::danger`] but without arming: for items that open
    /// their own confirmation.
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// The next enabled item from `from` (none: the first or last), wrapping.
fn next_enabled(enabled: &[bool], from: Option<usize>, down: bool) -> Option<usize> {
    let n = enabled.len();
    let mut i = from;
    for _ in 0..n {
        let next = match (i, down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(k), true) => (k + 1) % n,
            (Some(k), false) => (k + n - 1) % n,
        };
        if enabled[next] {
            return Some(next);
        }
        i = Some(next);
    }
    None
}

/// The index of item `i` among the list's children: a separator is a child
/// of its own in front of the item it rules off.
fn child_index(items: &[MenuItem], i: usize) -> usize {
    i + items
        .iter()
        .take(i + 1)
        .skip(1)
        .filter(|m| m.separated)
        .count()
}

/// The kebab button that holds a row's secondary actions (UX plan §4: at most
/// one visible row button, the rest in here). The menu is a [`Popover`], so
/// it opens over the table card's edge instead of being cut off by it.
///
/// Keyboard: ↑↓ Home End move, Enter/Space picks, Esc and Tab close. A
/// danger item asks in place: its first pick turns it into "<label>? ✓", the
/// second runs it, and while armed its `title` (what follows) shows under
/// it; closing the menu disarms it. The second press of a
/// double-click (or a held Enter) is not that second pick. Something that
/// deletes what a modal should explain first (a model, an upstream) is a
/// [`MenuItem::destructive`] item that opens that modal instead.
#[component]
pub fn RowMenu(
    #[prop(into)] items: Signal<Vec<MenuItem>>,
    /// The button's tooltip.
    #[prop(default = "More actions")]
    title: &'static str,
    /// Words instead of the kebab button, for a menu that is an action of its own ("+ add
    /// file role (9 more)") rather than a row's overflow.
    #[prop(optional, into)]
    label: Option<Signal<String>>,
    /// A symbolic icon on a header bar's round button instead of the kebab (the
    /// detail pane's more-menu, a list's add menu). Given with `label`, the
    /// icon leads the words on a text button.
    #[prop(optional)]
    icon: Option<super::icon::Ico>,
    /// A disclosure chevron after the words: a menu button that says it opens
    /// a list. For a button with a `label`.
    #[prop(optional)]
    chevron: bool,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let btn: NodeRef<html::Button> = NodeRef::new();
    let list: NodeRef<html::Div> = NodeRef::new();
    let active = RwSignal::new(None::<usize>);
    let armed = RwSignal::new(None::<usize>);
    let armed_at = StoredValue::new(0.0_f64);
    let was_open = StoredValue::new(false);

    Effect::new(move |_| {
        if !open.get() {
            armed.set(None);
            active.set(None);
        }
    });

    let step = move |from: Option<usize>, down: bool| {
        let flags: Vec<bool> = items.with_untracked(|v| v.iter().map(|m| !m.disabled).collect());
        next_enabled(&flags, from, down)
    };
    let focus_btn = move || {
        if let Some(b) = btn.get_untracked() {
            let _ = b.focus();
        }
    };
    // `detail`: the click count of a mouse pick, 0 from the keyboard.
    let choose = move |i: usize, detail: i32| {
        let Some(item) = items.with_untracked(|v| v.get(i).cloned()) else {
            return;
        };
        if item.disabled {
            return;
        }
        if item.danger {
            if armed.get_untracked() != Some(i) {
                armed_at.set_value(js_sys::Date::now());
                armed.set(Some(i));
                active.set(Some(i));
                return;
            }
            if is_second_press(detail, js_sys::Date::now() - armed_at.get_value()) {
                return;
            }
        }
        open.set(false);
        focus_btn();
        item.on_select.run(());
    };
    let set_active = move |i: Option<usize>| {
        active.set(i);
        if let (Some(i), Some(l)) = (i, list.get_untracked()) {
            popover::reveal_child(&l, items.with_untracked(|v| child_index(v, i)));
        }
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        let cur = active.get_untracked();
        match ev.key().as_str() {
            "ArrowDown" => set_active(step(cur, true)),
            "ArrowUp" => set_active(step(cur, false)),
            "Home" => set_active(step(None, true)),
            "End" => set_active(step(None, false)),
            "Enter" | " " => {
                if let Some(i) = cur {
                    if !ev.repeat() {
                        choose(i, 0);
                    }
                }
            }
            "Escape" => {
                ev.stop_propagation();
                open.set(false);
                focus_btn();
            }
            "Tab" => {
                open.set(false);
                focus_btn();
                return;
            }
            _ => return,
        }
        ev.prevent_default();
    };

    view! {
        <span class="row-menu">
            <button
                type="button"
                class=if label.is_some() && chevron {
                    "btn ghost sm menu-btn"
                } else if label.is_some() {
                    "btn ghost sm"
                } else if icon.is_some() {
                    "icon-btn"
                } else {
                    "btn ghost sm row-menu-btn"
                }
                node_ref=btn
                title=title
                aria-haspopup="menu"
                aria-expanded=move || open.get().to_string()
                on:pointerdown=move |_| was_open.set_value(open.get_untracked())
                on:click=move |_| {
                    let reopen = !was_open.get_value();
                    was_open.set_value(false);
                    open.set(reopen && !open.get_untracked());
                }
                on:keydown=move |ev| {
                    if !open.get_untracked() && matches!(ev.key().as_str(), "ArrowDown" | "ArrowUp") {
                        ev.prevent_default();
                        open.set(true);
                    }
                }
            >
                {match (icon, label) {
                    (icon, Some(l)) => {
                        view! {
                            {icon.map(|i| view! { <super::icon::Icon ico=i class="menu-btn-lead"/> })}
                            {move || l.get()}
                            {chevron
                                .then(|| {
                                    view! {
                                        <super::icon::Icon
                                            ico=super::icon::Ico::Down
                                            class="menu-btn-ico"
                                        />
                                    }
                                })}
                        }
                            .into_any()
                    }
                    (Some(ico), None) => view! { <super::icon::Icon ico=ico/> }.into_any(),
                    (None, None) => view! { <super::icon::Icon ico=super::icon::Ico::More/> }.into_any(),
                }}
            </button>
            <Popover open=open anchor=btn class="menu-pop">
                <div
                    class="pop-list menu-list"
                    role="menu"
                    tabindex="-1"
                    node_ref=list
                    data-autofocus
                    on:keydown=on_key
                >
                    {move || {
                        items
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, m)| {
                                let is_armed = move || armed.get() == Some(i);
                                let label = m.label.clone();
                                // An armed danger item says what follows in
                                // the menu itself, not only in its tooltip.
                                let note = m.danger.then(|| m.title.clone()).flatten();
                                view! {
                                    {(m.separated && i > 0)
                                        .then(|| view! { <div class="menu-sep" role="separator"></div> })}
                                    <div
                                        class="menu-item"
                                        class:danger=m.danger || m.destructive
                                        class:armed=is_armed
                                        class:active=move || active.get() == Some(i)
                                        class:disabled=m.disabled
                                        role="menuitem"
                                        aria-disabled=m.disabled.then_some("true")
                                        title=m.title.clone()
                                        on:pointermove=move |_| {
                                            if !m.disabled && active.get_untracked() != Some(i) {
                                                active.set(Some(i));
                                            }
                                        }
                                        on:click=move |ev| choose(i, ev.detail())
                                    >
                                        {move || {
                                            if is_armed() {
                                                format!("{label}? ✓")
                                            } else {
                                                label.clone()
                                            }
                                        }}
                                        {move || {
                                            note.clone()
                                                .filter(|_| is_armed())
                                                .map(|n| view! { <div class="menu-item-note">{n}</div> })
                                        }}
                                    </div>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Popover>
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(separated: bool) -> MenuItem {
        let m = MenuItem::new("x", || {});
        if separated {
            m.separated()
        } else {
            m
        }
    }

    #[test]
    fn arrow_down_crosses_a_separator() {
        let enabled = [true, true, true];
        assert_eq!(next_enabled(&enabled, Some(0), true), Some(1));
        assert_eq!(next_enabled(&enabled, Some(2), true), Some(0));
        assert_eq!(next_enabled(&[true, false, true], Some(0), true), Some(2));
        assert_eq!(next_enabled(&enabled, None, false), Some(2));
        assert_eq!(next_enabled(&[false, false], None, true), None);
    }

    #[test]
    fn a_separator_shifts_the_child_index_of_later_items_only() {
        let items = [item(true), item(false), item(true), item(false)];
        // The first item's own flag draws no rule.
        assert_eq!(child_index(&items, 0), 0);
        assert_eq!(child_index(&items, 1), 1);
        assert_eq!(child_index(&items, 2), 3);
        assert_eq!(child_index(&items, 3), 4);
    }
}
