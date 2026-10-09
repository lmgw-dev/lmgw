//! How long a test waits for something before it fails, and what it says
//! while it waits: the harness's own figures, read from
//! `.config/nextest.toml` as nextest reads them, never a guess of how fast
//! this machine is.
//!
//! nextest calls a test SLOW once it has run `slow-timeout.period`, and
//! again each period after, and ends it once it has run `period ×
//! terminate-after` (five minutes today). A wait cut shorter than that
//! fails a correct test whenever a loaded box is slower than the guess —
//! the fixed five-second polls these waits replaced did, under parallel
//! gates. So a wait gets the whole budget: on a working build it ends when
//! its condition holds, which is at once.
//!
//! On a broken one, nextest's timeout ends the test before the wait gives
//! up: the test's budget began with the test, a wait's with the wait. So a
//! wait says on stderr what it waits for, and what it saw last where it has
//! something to show, each time another period has passed — nextest prints
//! a timed-out test's output, and the last of those lines names what never
//! came. Under plain `cargo test` nothing ends a test, and a wait that has
//! had the whole budget panics with the same words. A config whose
//! slow-timeout has no `terminate-after` ends no test, and then no wait
//! gives up either.

use std::fmt::Display;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The file the figures are read from, as nextest reads it.
const NEXTEST_TOML: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.config/nextest.toml"
));

/// How often the polls below look again.
const POLL: Duration = Duration::from_millis(10);

/// `profile.default.slow-timeout`, the setting nextest runs these tests
/// under.
#[derive(Debug, PartialEq, Eq)]
struct SlowTimeout {
    /// How long a test runs before nextest calls it slow, and again each
    /// time it is still running.
    period: Duration,
    /// After how many periods nextest ends it; `None`: never.
    terminate_after: Option<u32>,
}

impl SlowTimeout {
    /// `profile.default.slow-timeout` of a nextest config, in either of its
    /// forms: `"60s"`, a period alone, or a table with `period`,
    /// `terminate-after` and `grace-period` (how long nextest waits between
    /// its SIGTERM and its SIGKILL: nothing to read here).
    fn read(config: &str) -> Self {
        let config: toml::Table = config
            .parse()
            .unwrap_or_else(|e| panic!("the nextest config does not parse: {e}"));
        let setting = config
            .get("profile")
            .and_then(|p| p.get("default"))
            .and_then(|d| d.get("slow-timeout"))
            .expect("the nextest config sets profile.default.slow-timeout");
        let read = match setting {
            toml::Value::String(period) => Self {
                period: duration(period),
                terminate_after: None,
            },
            toml::Value::Table(t) => Self {
                period: duration(
                    t.get("period")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_else(|| panic!("slow-timeout has no period: {t}")),
                ),
                terminate_after: t.get("terminate-after").map(|n| {
                    n.as_integer()
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or_else(|| panic!("slow-timeout's terminate-after is {n}"))
                }),
            },
            other => panic!("slow-timeout is neither a duration nor a table: {other}"),
        };
        assert!(
            read.period > Duration::ZERO,
            "slow-timeout's period is zero"
        );
        read
    }

    /// How long nextest lets one test run; `None`: as long as it takes.
    fn budget(&self) -> Option<Duration> {
        self.terminate_after.map(|n| self.period * n)
    }
}

/// A duration as nextest's config writes one (`"30s"`, `"1m 30s"`,
/// `"500ms"`); a unit not read here fails loudly rather than being misread.
fn duration(text: &str) -> Duration {
    let mut total = Duration::ZERO;
    let mut rest = text.trim();
    assert!(!rest.is_empty(), "an empty duration");
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let n: u64 = rest[..digits]
            .parse()
            .unwrap_or_else(|e| panic!("`{text}` is not a duration: {e}"));
        rest = &rest[digits..];
        let unit = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        total += match &rest[..unit] {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n * 60),
            "h" => Duration::from_secs(n * 60 * 60),
            other => panic!("`{text}`: the unit `{other}` is not read here (ms, s, m, h)"),
        };
        rest = rest[unit..].trim_start();
    }
    total
}

/// The harness's setting, read once.
fn harness() -> &'static SlowTimeout {
    static READ: OnceLock<SlowTimeout> = OnceLock::new();
    READ.get_or_init(|| SlowTimeout::read(NEXTEST_TOML))
}

/// One wait for a condition (module doc), for a loop the helpers below do
/// not fit: it says what it waits for once per period, and how long it may
/// go on.
pub struct Wait {
    what: String,
    began: Instant,
    /// The periods it has said it was still waiting through.
    said: u32,
}

impl Wait {
    /// A wait for `what`, beginning now.
    pub fn new(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            began: Instant::now(),
            said: 0,
        }
    }

    /// The condition does not hold yet, and `seen` is what there is
    /// instead: said on stderr each time another period has passed.
    /// `false` once the wait has had the test's whole budget.
    pub fn goes_on(&mut self, seen: Option<&dyn Display>) -> bool {
        let harness = harness();
        let waited = self.began.elapsed();
        let periods =
            u32::try_from(waited.as_nanos() / harness.period.as_nanos()).unwrap_or(u32::MAX);
        if periods > self.said {
            self.said = periods;
            match seen {
                Some(seen) => eprintln!(
                    "{}: not yet after {waited:.1?}; last seen: {seen}",
                    self.what
                ),
                None => eprintln!("{}: not yet after {waited:.1?}", self.what),
            }
        }
        harness.budget().is_none_or(|b| waited < b)
    }

    /// [`Self::goes_on`], panicking with what it waited for and `seen` once
    /// the budget is spent; then the poll interval.
    pub async fn again(&mut self, seen: Option<&dyn Display>) {
        if !self.goes_on(seen) {
            self.give_up(seen);
        }
        tokio::time::sleep(POLL).await;
    }

    fn give_up(&self, seen: Option<&dyn Display>) -> ! {
        let budget = harness()
            .budget()
            .expect("a wait gives up only when the harness ends tests");
        match seen {
            Some(seen) => panic!(
                "{}: not within the test's budget of {budget:?}; last seen: {seen}",
                self.what
            ),
            None => panic!("{}: not within the test's budget of {budget:?}", self.what),
        }
    }
}

/// Poll `cond` until it holds, as a [`Wait`] for `what`.
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let mut wait = Wait::new(what);
    while !cond() {
        wait.again(None).await;
    }
}

/// [`until`] for a condition that has to be awaited — one read over HTTP,
/// say.
pub async fn until_async<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut wait = Wait::new(what);
    while !cond().await {
        wait.again(None).await;
    }
}

/// Await `fut`, which brings `what`, as a [`Wait`] would poll for it: said
/// on stderr once per period it takes, and a panic once it has taken the
/// whole budget.
pub async fn within<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    let mut wait = Wait::new(what);
    tokio::pin!(fut);
    loop {
        if let Ok(out) = tokio::time::timeout(harness().period, &mut fut).await {
            return out;
        }
        if !wait.goes_on(None) {
            wait.give_up(None);
        }
    }
}

#[test]
fn the_slow_timeout_is_read_in_both_of_nextests_forms() {
    let read = |setting: &str| SlowTimeout::read(&format!("[profile.default]\n{setting}\n"));
    assert_eq!(
        read(r#"slow-timeout = { period = "30s", terminate-after = 10 }"#),
        SlowTimeout {
            period: Duration::from_secs(30),
            terminate_after: Some(10),
        }
    );
    // grace-period is a duration of its own: not the period, wherever it
    // stands.
    assert_eq!(
        read(r#"slow-timeout = { grace-period = "5s", period = "1m 30s", terminate-after = 2 }"#),
        SlowTimeout {
            period: Duration::from_secs(90),
            terminate_after: Some(2),
        }
    );
    // The string form: a period, and no test is ended.
    let alone = read(r#"slow-timeout = "500ms""#);
    assert_eq!(
        alone,
        SlowTimeout {
            period: Duration::from_millis(500),
            terminate_after: None,
        }
    );
    assert_eq!(alone.budget(), None);
}

#[test]
fn the_slow_timeout_read_is_the_default_profiles() {
    let config = r#"
[profile.ci]
slow-timeout = { period = "1s", terminate-after = 1 }

[profile.default.slow-timeout]
period = "2m"
terminate-after = 3
grace-period = "10s"
"#;
    let read = SlowTimeout::read(config);
    assert_eq!(read.period, Duration::from_secs(120));
    assert_eq!(read.budget(), Some(Duration::from_secs(360)));
}

#[test]
fn this_workspaces_config_reads() {
    harness();
}
