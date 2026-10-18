//! A `noul` question's criteria (labs-store design §2.3): what a yes means
//! and what a no means, both optional; both blank sends no `criteria`.

use leptos::prelude::*;

use super::page::Lab;

#[component]
pub(super) fn NoulEditor(lab: Lab, uid: u32) -> impl IntoView {
    let f = lab.form;
    let (yes, no) = untrack(|| {
        f.questions
            .with(|v| {
                v.iter()
                    .find(|q| q.uid == uid)
                    .map(|q| (q.yes.clone(), q.no.clone()))
            })
            .unwrap_or_default()
    });
    view! {
        <div class="field-grid dl-noul">
            <div class="field">
                <label>"A yes means"</label>
                <input
                    class="input"
                    placeholder="optional"
                    prop:value=yes
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        f.edit(uid, |q| q.yes = v);
                    }
                />
            </div>
            <div class="field">
                <label>"A no means"</label>
                <input
                    class="input"
                    placeholder="optional"
                    prop:value=no
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        f.edit(uid, |q| q.no = v);
                    }
                />
            </div>
        </div>
        <p class="mini-note dim">
            "Both blank sends no criteria. The answer is P(yes); \"descriptions dropped\" leaves the criteria out."
        </p>
    }
}
