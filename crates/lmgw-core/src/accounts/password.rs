//! Passwords (headless server design §5.2, D12, §5.10; rulings 16, 20).
//!
//! - At least [`PASSWORD_MIN_CHARS`] characters, counted as Unicode code
//!   points after NFKC normalization ([`normalize`]), which is also what is
//!   hashed. No composition rules, no blocklist (ruling 20).
//! - argon2id with RFC 9106's second recommended parameter set (t = 3,
//!   p = 4, m = 64 MiB), stored as a PHC string; a hash made with other
//!   parameters is rehashed at the next good sign-in.
//! - Verification on the blocking pool, at most `available_parallelism()`
//!   at once ([`Verifier`]), one of them kept for known browsers; a sign-in
//!   that finds no permit free is told so (`503 login_busy`), never queued
//!   silently.

use std::sync::LazyLock;
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use unicode_normalization::UnicodeNormalization;

/// The shortest password (ruling 16), in code points after NFKC.
pub const PASSWORD_MIN_CHARS: usize = 12;

/// argon2's memory per verification, in KiB (RFC 9106 §4, the second
/// recommended option: 64 MiB).
pub const ARGON2_M_KIB: u32 = 64 * 1024;
/// Passes over the memory.
pub const ARGON2_T: u32 = 3;
/// Lanes.
pub const ARGON2_P: u32 = 4;

/// The one normalization a password goes through, before it is counted
/// and before it is hashed (ruling 20: NFKC).
///
/// NIST SP 800-63B revision 4 names NFC (SHOULD); NFKC also folds
/// compatibility characters (full-width letters, ligatures, superscripts)
/// into their plain forms, so two passwords that look different can verify
/// alike. NFKC is the design's choice (§5.2); this is the only place that
/// would change if it moved.
pub fn normalize(password: &str) -> String {
    password.nfkc().collect()
}

/// `password` normalized and checked against the minimum: the text to hash,
/// or the refusal naming the minimum.
pub fn checked(password: &str) -> Result<String, String> {
    let n = normalize(password);
    let chars = n.chars().count();
    if chars < PASSWORD_MIN_CHARS {
        return Err(format!(
            "a password has at least {PASSWORD_MIN_CHARS} characters; this one has {chars}"
        ));
    }
    Ok(n)
}

fn params() -> Params {
    Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, None).expect("RFC 9106's parameter set is valid")
}

fn argon2() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params())
}

/// The PHC string of an already [`checked`] password, with a fresh salt.
/// Blocking: about one verification's time.
pub fn hash(normalized: &str) -> Result<String, String> {
    argon2()
        .hash_password(normalized.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing the password failed: {e}"))
}

/// What a verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// The password matched; `Some` carries a fresh hash when the stored one
    /// was made with other parameters (D12's rehash).
    Good(Option<String>),
    Bad,
}

/// Verify `password` (normalized here) against the PHC string `stored`.
/// `None` runs the same work against a fixed dummy hash and answers
/// [`Verified::Bad`], so an unknown, disabled or password-less name costs
/// what a wrong password does (D13). Blocking.
pub fn verify(password: &str, stored: Option<&str>) -> Verified {
    let normalized = normalize(password);
    let Some(stored) = stored else {
        let _ = argon2().verify_password(normalized.as_bytes(), DUMMY.as_str());
        return Verified::Bad;
    };
    let Ok(parsed) = PasswordHash::new(stored) else {
        tracing::warn!("a stored password hash does not parse as a PHC string");
        let _ = argon2().verify_password(normalized.as_bytes(), DUMMY.as_str());
        return Verified::Bad;
    };
    // The stored hash's own parameters verify it; only a good one is
    // rehashed with today's.
    if Argon2::default()
        .verify_password(normalized.as_bytes(), &parsed)
        .is_err()
    {
        return Verified::Bad;
    }
    let current = Params::try_from(&parsed).ok().is_some_and(|p| {
        p.m_cost() == ARGON2_M_KIB && p.t_cost() == ARGON2_T && p.p_cost() == ARGON2_P
    }) && parsed.algorithm.as_str() == "argon2id";
    Verified::Good((!current).then(|| hash(&normalized).ok()).flatten())
}

/// The dummy hash D13's verification of a name without a password runs
/// against: made once per process, of a random password nobody knows.
static DUMMY: LazyLock<String> = LazyLock::new(|| {
    hash(&hex::encode(rand::random::<[u8; 32]>())).expect("hashing a random password works")
});

/// The verification permits (§5.10): at most `available_parallelism()`
/// verifications at once, each taking [`ARGON2_M_KIB`] and a blocking
/// thread. One of them is kept for the accounts' known browsers (§5.4)
/// whenever there are two or more, so a flood of sign-ins from unknown
/// browsers cannot keep an account's own browser out; a machine with one
/// core has none to keep.
pub struct Verifier {
    /// What every verification may take.
    general: std::sync::Arc<Semaphore>,
    /// What only a known browser's sign-in may take, once `general` is gone.
    known: std::sync::Arc<Semaphore>,
    count: usize,
    kept: usize,
}

impl Default for Verifier {
    fn default() -> Self {
        let count = std::thread::available_parallelism().map_or(1, |n| n.get());
        let kept = usize::from(count >= 2);
        Self {
            general: std::sync::Arc::new(Semaphore::new(count - kept)),
            known: std::sync::Arc::new(Semaphore::new(kept)),
            count,
            kept,
        }
    }
}

/// [`busy`]'s code.
pub const BUSY_CODE: &str = "login_busy";

/// The answer while every permit is taken: `503 login_busy`.
pub fn busy(count: usize) -> crate::principal::Refusal {
    crate::principal::Refusal {
        status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
        code: BUSY_CODE,
        message: format!("{count} sign-ins are being checked right now; try again in a moment"),
    }
}

impl Verifier {
    /// How many verifications may run at once, the kept one included.
    pub fn count(&self) -> usize {
        self.count
    }

    /// How many of them only a known browser may take.
    pub fn kept_for_known(&self) -> usize {
        self.kept
    }

    /// The memory they can take together, in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.count as u64 * u64::from(ARGON2_M_KIB) * 1024
    }

    /// A permit for any verification, or `None` when every one but the
    /// known browsers' is taken.
    pub fn try_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.general.clone().try_acquire_owned().ok()
    }

    /// A permit for a known browser's sign-in: any free one, the kept one
    /// included.
    pub fn try_permit_known(&self) -> Option<OwnedSemaphorePermit> {
        self.try_permit()
            .or_else(|| self.known.clone().try_acquire_owned().ok())
    }

    /// Verify on the blocking pool under `permit`, and measure it.
    pub async fn verify(
        &self,
        permit: OwnedSemaphorePermit,
        password: String,
        stored: Option<String>,
    ) -> (Verified, Duration) {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let started = std::time::Instant::now();
            let v = verify(&password, stored.as_deref());
            (v, started.elapsed())
        })
        .await
        .unwrap_or((Verified::Bad, Duration::ZERO))
    }

    /// Make the dummy hash D13's verification of an unknown name runs
    /// against now, on the blocking pool, so the first unknown name's
    /// sign-in does not also pay for hashing it (which would tell it from a
    /// known one by its time).
    pub fn warm(&self) {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn_blocking(|| LazyLock::force(&DUMMY).len());
        } else {
            LazyLock::force(&DUMMY);
        }
    }

    /// The start's log line (D12): how many at once and what they can take;
    /// and the dummy hash made ([`Self::warm`]).
    pub fn log_at_start(&self) {
        self.warm();
        tracing::info!(
            "accounts: up to {} password verifications at once ({} of them kept for known \
             browsers), {} of memory together (argon2id t={ARGON2_T} p={ARGON2_P} m={})",
            self.count,
            self.kept,
            lmgw_api_types::units::fmt_bytes(self.memory_bytes()),
            lmgw_api_types::units::fmt_bytes(u64::from(ARGON2_M_KIB) * 1024),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_code_points_after_nfkc_and_no_blocklist() {
        // 11 refused, naming the minimum.
        let e = checked("abcdefghijk").unwrap_err();
        assert!(e.contains("at least 12"), "{e}");
        // 12 accepted, a commonly used one included (ruling 20).
        assert!(checked("password1234").is_ok());
        // Code points, not bytes: twelve 'ä' are 24 bytes and 12 characters.
        assert_eq!(checked(&"ä".repeat(12)).unwrap().chars().count(), 12);
        // NFKC: a ligature folds to two letters, full-width to ASCII.
        assert_eq!(normalize("ﬁ"), "fi");
        assert_eq!(normalize("ＡＢＣ"), "ABC");
        // A decomposed 'ä' (a + U+0308) composes to one code point.
        assert_eq!(normalize("a\u{0308}").chars().count(), 1);
    }

    #[test]
    fn a_hash_verifies_its_password_and_its_folded_twin() {
        let h = hash(&checked("correct horse battery").unwrap()).unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"), "{h}");
        assert_eq!(
            verify("correct horse battery", Some(&h)),
            Verified::Good(None)
        );
        assert_eq!(verify("correct horse batterx", Some(&h)), Verified::Bad);
        assert_eq!(verify("anything at all", None), Verified::Bad);
        // Full-width letters read as their plain forms (NFKC).
        let h = hash(&checked("ＣＯＲＲＥＣＴhorse1").unwrap()).unwrap();
        assert!(matches!(
            verify("CORRECThorse1", Some(&h)),
            Verified::Good(_)
        ));
    }

    #[test]
    fn a_hash_with_other_parameters_is_rehashed() {
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8 * 1024, 1, 1, None).unwrap(),
        )
        .hash_password(b"an old weak password")
        .unwrap()
        .to_string();
        match verify("an old weak password", Some(&weak)) {
            Verified::Good(Some(new)) => {
                assert!(new.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"));
                assert_eq!(
                    verify("an old weak password", Some(&new)),
                    Verified::Good(None)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// One verification's time at the parameters above (§5.10): run it
    /// with `cargo test --release --lib -p lmgw-core
    /// accounts::password::tests::one_verification_s_time -- --ignored
    /// --nocapture`. 2026-11-08, argon2 0.6.0 without `parallel`, a 16-core
    /// desktop CPU: 95 ms (min 94.6, max 99.5 of ten).
    #[test]
    #[ignore = "a measurement, run on demand in a release build"]
    fn one_verification_s_time() {
        let h = hash("correct horse battery staple").unwrap();
        let mut ms: Vec<f64> = (0..10)
            .map(|_| {
                let t = std::time::Instant::now();
                assert!(matches!(
                    verify("correct horse battery staple", Some(&h)),
                    Verified::Good(None)
                ));
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!(
            "argon2id t={ARGON2_T} p={ARGON2_P} m=64 MiB: min {:.1} ms, median {:.1} ms, max {:.1} ms",
            ms[0], ms[5], ms[9]
        );
    }

    #[test]
    fn permits_run_out_and_are_named_and_one_is_kept_for_known_browsers() {
        let v = Verifier::default();
        let general = v.count() - v.kept_for_known();
        let held: Vec<_> = (0..general).map(|_| v.try_permit().unwrap()).collect();
        assert!(v.try_permit().is_none());
        assert_eq!(busy(v.count()).status, 503);
        if v.count() >= 2 {
            assert_eq!(v.kept_for_known(), 1);
            let kept = v.try_permit_known().expect("the known browsers' permit");
            assert!(v.try_permit_known().is_none());
            drop(kept);
        }
        drop(held);
        assert!(v.try_permit().is_some());
    }
}
