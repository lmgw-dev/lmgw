//! The Chat's personality-profile pieces (personality-profiles design §4.1):
//! [`dir`], the list the pickers read, and [`pick`], the settings drawer's
//! field (with its link to the host's profile editor, where it has one) and
//! the thread header's chip. The editor itself is the host's page (the
//! dashboard's `/chat/profiles`, framing the kit's `ProfilesPanel`).

mod dir;
mod pick;

pub(crate) use dir::{use_profile_dir, ProfileDir};
pub(crate) use pick::{ProfileBadge, ProfileField};
