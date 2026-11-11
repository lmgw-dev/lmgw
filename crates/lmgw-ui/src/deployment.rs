//! Where the gateway runs (headless server design D2), for the widgets that
//! word something differently in the server image: read once from
//! `GET /api/settings-full`'s `deployment`, then kept for the page's life
//! (a gateway does not move between deployments while it runs). `None`
//! until the read came back: the desktop's words until then. A failed read
//! is asked again on a timer for as long as it fails, never given up.

use std::cell::RefCell;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::Deployment;
use serde_json::Value;

thread_local! {
    /// The one read's answer, shared by every caller.
    static READ: RefCell<Option<ArcRwSignal<Option<Deployment>>>> = const { RefCell::new(None) };
}

/// How long the read waits before it asks again after a failure: doubling
/// from the first, up to the last, and retried for as long as it fails.
const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two reads.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The gateway's deployment, reactive; the first call begins the read.
pub fn deployment() -> Signal<Option<Deployment>> {
    let read = READ.with_borrow_mut(|r| {
        r.get_or_insert_with(|| {
            let s = ArcRwSignal::new(None);
            ask(s.clone(), RETRY_FIRST);
            s
        })
        .clone()
    });
    Signal::derive(move || read.get())
}

/// Read the deployment into `into`; on a failure, ask again after `wait`,
/// and after twice as long (up to [`RETRY_MAX`]) each time it fails again.
fn ask(into: ArcRwSignal<Option<Deployment>>, wait: Duration) {
    spawn_local(async move {
        let answer = crate::api::get::<Value>("/api/settings-full")
            .await
            .ok()
            .and_then(|v| serde_json::from_value::<Deployment>(v["deployment"].clone()).ok());
        match answer {
            Some(d) => into.set(Some(d)),
            None => set_timeout(move || ask(into, (wait * 2).min(RETRY_MAX)), wait),
        }
    });
}

/// What a mount's empty box says: the shell's picker fills it; a browser
/// types an absolute path — on the gateway's machine, or in the server
/// image inside the lmgw container (headless server design §3.2).
pub fn mount_placeholder(shell: bool, deployment: Option<Deployment>) -> &'static str {
    if shell {
        "not set"
    } else if deployment.is_some_and(Deployment::is_container) {
        "absolute path inside the lmgw container"
    } else {
        "absolute path on the gateway machine"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mount's empty box: the shell's picker, a path on the gateway's
    /// machine, or one inside the lmgw container in the server image.
    #[test]
    fn a_mount_says_whose_path_it_takes() {
        assert_eq!(
            mount_placeholder(true, Some(Deployment::Desktop)),
            "not set"
        );
        assert_eq!(
            mount_placeholder(false, Some(Deployment::Desktop)),
            "absolute path on the gateway machine"
        );
        assert_eq!(
            mount_placeholder(false, None),
            "absolute path on the gateway machine"
        );
        assert_eq!(
            mount_placeholder(false, Some(Deployment::Container)),
            "absolute path inside the lmgw container"
        );
    }
}
