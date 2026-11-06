//! The hash (§6, "The hash"): stable across runs, changed by any argv or
//! file content, and over the dedicated struct only.

use lmgw_machine::plan::{HashedPlan, Want, PLAN_SCHEMA, PROVIDERS};

use super::machine::Machine;

/// The fresh-Fedora plan's hash, pinned: a change to any command, file
/// text, the key or the encoding moves it, and a moved hash is a reviewed
/// change (a helper of the new version would refuse the old app's plan,
/// which §3.5's schema line covers). Moved once in review: the steps run
/// `/usr/bin/dnf5`, the file the button's check found, not `/usr/bin/dnf`.
const FRESH_FEDORA: &str = "0e30c7334a48d0d8b3d029e399a479130dc3ba5eb306447dfecab4b626a546fe";

#[test]
fn the_hash_is_stable_and_pinned() {
    let a = Machine::fedora().plan();
    let b = Machine::fedora().plan();
    assert_eq!(a.hash(), b.hash());
    assert_eq!(a.hash().len(), 64);
    assert!(a
        .hash()
        .bytes()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(
        a.hash(),
        FRESH_FEDORA,
        "the fresh-Fedora plan changed: {:#?}",
        a.hashed()
    );
}

#[test]
fn any_argv_or_file_content_changes_the_hash() {
    let base = Machine::fedora().plan();
    let mut h = base.hashed();
    let original = h.sha256();
    h.steps[2].commands[0].push("--extra".into());
    assert_ne!(h.sha256(), original, "an argument");
    let mut h = base.hashed();
    h.steps[0].file.as_mut().unwrap().content.push('\n');
    assert_ne!(h.sha256(), original, "a file's content");
    let mut h = base.hashed();
    h.steps[0].file.as_mut().unwrap().replace = true;
    assert_ne!(h.sha256(), original, "create versus replace");
    let mut h = base.hashed();
    h.schema += 1;
    assert_ne!(h.sha256(), original, "the schema");
    // A step's arguments split differently are another plan, not the same
    // bytes.
    let mut h = base.hashed();
    let argv = &mut h.steps[1].commands[0];
    let last = argv.pop().unwrap();
    let before = argv.pop().unwrap();
    argv.push(format!("{before}{last}"));
    assert_ne!(h.sha256(), original, "argument boundaries");

    // Another section: other argv, another hash.
    let m = Machine::fedora();
    m.nvidia_section();
    assert_ne!(m.plan().hash(), original);
}

/// Only what runs as root is hashed: the struct has no sentence, no size,
/// no section origin; the user's wish to update changes it only through the
/// step it adds.
#[test]
fn the_hash_covers_the_dedicated_struct_only() {
    let p = Machine::fedora().plan();
    let json = serde_json::to_value(p.hashed()).unwrap();
    assert_eq!(json["schema"], PLAN_SCHEMA);
    let step = &json["steps"][0];
    let mut keys: Vec<&str> = step
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(keys, ["commands", "file", "kind"]);
    // The hash is HashedPlan's, recomputed from the steps alone.
    assert_eq!(p.hash(), HashedPlan::of(&p.steps).sha256());
    // What the helper would print on a mismatch reads back as the same plan.
    let back: HashedPlan = serde_json::from_value(json).unwrap();
    assert_eq!(back.sha256(), p.hash());
    // A machine that needs no update has the same plan with the flag set.
    let m = Machine::fedora();
    assert_eq!(
        m.plan().hash(),
        m.plan_with(
            Want {
                update_toolkit: true
            },
            PROVIDERS
        )
        .hash()
    );
}

/// The plan's inputs are read through root's environment on the user's side
/// too (D87): a `~/.rpmmacros` whose `%_keyringpath` or `%_dbpath` moves
/// rpm's answers as the user (here: NVIDIA's key trusted) leaves the plan
/// root computes, so the hash the person confirms is one the helper accepts.
#[test]
fn the_plan_reads_through_roots_environment() {
    let m = Machine::fedora();
    let roots = m.plan();
    m.f.program_in_users_env("/usr/bin/rpmkeys", |_| {
        Ok(lmgw_machine::CmdOut::success(format!(
            "{} NVIDIA CORPORATION public key\n",
            lmgw_machine::plan::embedded::NVIDIA_KEY_FINGERPRINT
        )))
    });
    // As the user's environment answers, the machine looks otherwise.
    let users_keys =
        lmgw_machine::Facts::run(&m.f, std::path::Path::new("/usr/bin/rpmkeys"), &["--list"])
            .unwrap();
    assert!(users_keys
        .stdout
        .contains(lmgw_machine::plan::embedded::NVIDIA_KEY_FINGERPRINT));
    // A machine that really is as the user's environment answers has
    // another plan (lmgw's key file without its import).
    let as_the_user_sees_it = Machine::fedora();
    as_the_user_sees_it.trust_nvidia();
    assert_ne!(as_the_user_sees_it.plan().hash(), roots.hash());
    // This one's plan is root's.
    assert_eq!(m.plan().hash(), roots.hash());
    assert_eq!(m.inputs(), Machine::fedora().inputs());
}
