//! The polkit action the RPM ships (§3.3): its id, its defaults and the one
//! program it authorises. ci/rpm-allowlist.toml pins the file's sha256, so a
//! change here is a change there too (ci/rpm_guard_test.py compares them).

use lmgw_machine::plan::embedded::{HELPER, POLICY};

const TEXT: &str = include_str!("../../polkit/io.github.lmgw-dev.lmgw.machine-setup.policy");
const ACTION: &str = "io.github.lmgw-dev.lmgw.machine-setup";

/// The text of every `<tag>…</tag>` in `text`, in order.
fn values<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        let end = after.find(&close).expect("a closing tag");
        out.push(after[..end].trim());
        rest = &after[end + close.len()..];
    }
    out
}

#[test]
fn the_action_asks_an_administrator_every_time_and_only_where_one_sits() {
    // One action, and the defaults of §3.3: never without a password, never
    // from an inactive or remote session, and no kept authorisation
    // (auth_admin, not auth_admin_keep: D1).
    assert_eq!(TEXT.matches("<action ").count(), 1);
    assert!(TEXT.contains(&format!("<action id=\"{ACTION}\">")));
    assert_eq!(values(TEXT, "allow_any"), ["no"]);
    assert_eq!(values(TEXT, "allow_inactive"), ["no"]);
    assert_eq!(values(TEXT, "allow_active"), ["auth_admin"]);
    for tag in ["allow_any", "allow_inactive", "allow_active"] {
        for v in values(TEXT, tag) {
            assert!(!v.contains("yes") && !v.contains("keep"), "{tag}: {v}");
        }
    }
}

/// The action is the helper's `apply` alone: pkexec matches an action by
/// its program and, with `exec.argv1`, by the first argument too, so
/// `pkexec helper plan` or `run` gets polkit's generic "run as the super
/// user" action and its wording, never this action's message.
#[test]
fn the_action_authorises_the_helpers_apply_alone() {
    let annotations: Vec<&str> = TEXT.matches("<annotate ").collect();
    assert_eq!(annotations.len(), 2, "exec.path and exec.argv1 only");
    assert!(TEXT.contains(&format!(
        "<annotate key=\"org.freedesktop.policykit.exec.path\">{HELPER}</annotate>"
    )));
    assert!(
        TEXT.contains("<annotate key=\"org.freedesktop.policykit.exec.argv1\">apply</annotate>")
    );
    assert_eq!(HELPER, "/usr/libexec/lmgw/lmgw-machine-helper");
}

#[test]
fn the_policy_is_installed_under_its_actions_name() {
    assert_eq!(
        POLICY,
        format!("/usr/share/polkit-1/actions/{ACTION}.policy")
    );
    assert_eq!(values(TEXT, "icon_name"), ["lmgw"]);
    // What the password window says, true of every plan the button offers
    // (a fresh install, a switched-on section, the toolkit update, the swap
    // of Fedora's retired package), and not "your password": an account that
    // is not an administrator is asked for an administrator's (§3.3).
    assert_eq!(
        values(TEXT, "message"),
        [
            "lmgw wants to set up the graphics card for local models: install or update NVIDIA's \
          container toolkit and its software source."
        ]
    );
}
