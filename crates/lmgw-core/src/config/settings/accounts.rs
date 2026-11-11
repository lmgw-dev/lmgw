//! The `accounts` section (headless server design §5, ruling 17, D29):
//! `accounts.enabled`, the desktop's switch, and `accounts.session_days`,
//! a session's lifetime, and `accounts.audit_days`, how long the audit
//! log keeps an entry (§5.8).
//!
//! `enabled` is written only by the `accounts_switch` op: every settings
//! save path refuses it with [`refuse_accounts_enabled`], because a field
//! write would skip what the switch does to sessions and keys (§5.11). In
//! container mode accounts are always on, whatever it says.

use lmgw_api_types::settings_labels::{AUDIT_KEEP_AT, SESSION_LIFETIME_AT, SIGN_IN_ACCOUNTS_AT};
use serde::{Deserialize, Serialize};

/// `accounts.session_days`'s default (ruling 19: 30 days).
pub const SESSION_DAYS_DEFAULT: u32 = 30;

/// The longest lifetime `accounts.session_days` takes: ten years, which
/// keeps every expiry a date `chrono` writes.
pub const SESSION_DAYS_MAX: u32 = 3650;

/// The `accounts` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AccountsSettings {
    /// The desktop's switch (ruling 17): off by default. Written only by
    /// `accounts_switch`.
    pub enabled: bool,
    /// How long a session and a known browser last, in days (ruling 19).
    pub session_days: u32,
    /// How long the audit log keeps an entry, in days (§5.8); 0, the
    /// default, keeps every entry. A change is written to the log before
    /// it prunes anything (ruling 18).
    pub audit_days: u32,
}

impl Default for AccountsSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            session_days: SESSION_DAYS_DEFAULT,
            audit_days: 0,
        }
    }
}

/// `accounts.session_days` checked: 1 to [`SESSION_DAYS_MAX`].
pub fn validate_session_days(days: u32) -> Result<u32, String> {
    if (1..=SESSION_DAYS_MAX).contains(&days) {
        Ok(days)
    } else {
        Err(format!(
            "{SESSION_LIFETIME_AT} must be 1 to {SESSION_DAYS_MAX} days (got {days})"
        ))
    }
}

/// `accounts.audit_days` checked: 0 (keep every entry) to
/// [`SESSION_DAYS_MAX`].
pub fn validate_audit_days(days: u32) -> Result<u32, String> {
    if days <= SESSION_DAYS_MAX {
        Ok(days)
    } else {
        Err(format!(
            "{AUDIT_KEEP_AT} must be 0 (keep every entry) to {SESSION_DAYS_MAX} days (got {days})"
        ))
    }
}

/// The code every settings save path refuses `accounts.enabled` with.
pub const ACCOUNTS_SWITCH_CODE: &str = "accounts_switch_only";

/// The sentence of that refusal (D29).
pub fn accounts_switch_only() -> String {
    format!(
        "accounts are switched on and off only by {SIGN_IN_ACCOUNTS_AT} (the accounts_switch \
         op), which also ends the sessions and pauses or resumes the keys the change concerns; \
         a settings save does not change it"
    )
}

/// The shared settings write's refusal of `accounts.enabled` (D29), for a
/// save whose patch carries it.
pub fn refuse_accounts_enabled() -> crate::principal::Refusal {
    crate::principal::Refusal {
        status: axum::http::StatusCode::BAD_REQUEST,
        code: ACCOUNTS_SWITCH_CODE,
        message: accounts_switch_only(),
    }
}

/// A settings patch's `accounts` part: `session_days` is an ordinary
/// setting; `enabled` is there only so a save that carries it is refused
/// by name rather than as an unknown field.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsSettingsPatch {
    /// Refused by every save path: the accounts_switch op changes it.
    pub enabled: Option<serde_json::Value>,
    /// How long a session lasts, in days: 1 to 3650. Applies to sessions
    /// signed in from now on.
    pub session_days: Option<u32>,
    /// How many days the audit log keeps an entry: 0 keeps every entry, up
    /// to 3650. The change is written to the log before it prunes.
    pub audit_days: Option<u32>,
}

impl AccountsSettingsPatch {
    /// Apply it to `s`: refused when it carries `enabled` (even unchanged),
    /// the changed key names otherwise.
    pub fn apply(
        self,
        s: &mut AccountsSettings,
    ) -> Result<Vec<&'static str>, crate::principal::Refusal> {
        if self.enabled.as_ref().is_some_and(|v| !v.is_null()) {
            return Err(refuse_accounts_enabled());
        }
        let mut changed = Vec::new();
        if let Some(days) = self.session_days {
            s.session_days = validate_session_days(days)?;
            changed.push("accounts.session_days");
        }
        if let Some(days) = self.audit_days {
            s.audit_days = validate_audit_days(days)?;
            changed.push("accounts.audit_days");
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_is_refused_and_session_days_checked() {
        let mut s = AccountsSettings::default();
        let p = AccountsSettingsPatch {
            enabled: Some(serde_json::json!(true)),
            ..Default::default()
        };
        assert_eq!(p.apply(&mut s).unwrap_err().code, ACCOUNTS_SWITCH_CODE);
        // Unchanged is still refused: the key itself is the switch's.
        let p = AccountsSettingsPatch {
            enabled: Some(serde_json::json!(false)),
            ..Default::default()
        };
        assert!(p.apply(&mut s).is_err());
        let p = AccountsSettingsPatch {
            enabled: None,
            session_days: Some(0),
            ..Default::default()
        };
        assert!(p.apply(&mut s).is_err());
        let p = AccountsSettingsPatch {
            enabled: None,
            session_days: Some(7),
            ..Default::default()
        };
        assert_eq!(p.apply(&mut s).unwrap(), vec!["accounts.session_days"]);
        assert_eq!(s.session_days, 7);
        assert!(!s.enabled);
        let p = AccountsSettingsPatch {
            audit_days: Some(3651),
            ..Default::default()
        };
        assert!(p.apply(&mut s).is_err());
        let p = AccountsSettingsPatch {
            audit_days: Some(0),
            ..Default::default()
        };
        assert_eq!(p.apply(&mut s).unwrap(), vec!["accounts.audit_days"]);
        assert_eq!(s.audit_days, 0);
    }
}
