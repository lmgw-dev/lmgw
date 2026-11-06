//! One call's hooks (chat-media-tools design §5, §6.2, §7.2): its caller's
//! claim hook, and its progress reported to whoever follows the call — the
//! Chat's `progress` frames, `/mcp`'s `notifications/progress` — with what
//! repeats left out.

use std::sync::Mutex;

use crate::config::Route;
use crate::erased::Erased;
use crate::proxy::image_call::{Follows, Hooks, MediaProgress};

/// Who follows a running call's progress.
pub(crate) trait Progress: Send + Sync {
    /// The call's progress now. Never waited on: a reader that falls
    /// behind drops what it cannot show (the next report supersedes it).
    fn report(&self, p: &MediaProgress);

    /// Whether it shows previews; otherwise an image job is asked only for
    /// what reports its steps.
    fn previews(&self) -> bool {
        false
    }
}

/// The caller's claim hook and the call's progress.
pub(super) struct CallHooks<'a> {
    claim: &'a dyn Hooks,
    progress: Option<&'a dyn Progress>,
    last: Mutex<Option<MediaProgress>>,
}

impl<'a> CallHooks<'a> {
    pub(super) fn new(claim: &'a dyn Hooks, progress: Option<&'a dyn Progress>) -> Self {
        Self {
            claim,
            progress,
            last: Mutex::new(None),
        }
    }
}

impl Hooks for CallHooks<'_> {
    fn before_admit<'a>(&'a self, route: &'a Route) -> Erased<'a, ()> {
        self.claim.before_admit(route)
    }

    fn progress(&self, p: &MediaProgress) {
        let Some(to) = self.progress else {
            return;
        };
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if !p.is_news_after(last.as_ref()) {
                return;
            }
            *last = Some(p.clone());
        }
        to.report(p);
    }

    fn follows(&self) -> Follows {
        match self.progress {
            Some(p) if p.previews() => Follows::Previews,
            _ => Follows::Steps,
        }
    }
}
