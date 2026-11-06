//! What a step's package changes by itself (§4.5): a note for the steps
//! that install NVIDIA's package through dnf, none for the others, and
//! nothing of it in the hash.

use lmgw_machine::plan::scripts::{INSTALL_SCRIPTS, UPDATE_SCRIPTS};
use lmgw_machine::plan::steps::toolkit_base;
use lmgw_machine::plan::{
    package_scripts, Installer, NonEmpty, PackageName, RepoId, Scope, Step, PLAN_SCHEMA,
};

use super::machine::Machine;

fn scope() -> Scope {
    Scope {
        repo: RepoId::new("lmgw-nvidia-container-toolkit").unwrap(),
        nvidia_key: true,
        include: vec![],
    }
}

#[test]
fn the_package_steps_say_what_the_package_changes_by_itself() {
    let install = Step::Install {
        via: Installer::Dnf(scope()),
        package: toolkit_base(),
    };
    let swap = Step::Swap {
        scope: scope(),
        from: PackageName::new("golang-github-nvidia-container-toolkit").unwrap(),
        to: toolkit_base(),
    };
    let update = Step::Update {
        scope: scope(),
        packages: NonEmpty::new(toolkit_base(), vec![]),
    };
    assert_eq!(package_scripts(&install), Some(INSTALL_SCRIPTS));
    assert_eq!(package_scripts(&swap), Some(INSTALL_SCRIPTS));
    assert_eq!(package_scripts(&update), Some(UPDATE_SCRIPTS));
    for text in [INSTALL_SCRIPTS, UPDATE_SCRIPTS] {
        for named in [
            "/etc/nvidia-container-runtime/config.toml",
            "nvidia-cdi-refresh",
            "/var/run/cdi/nvidia.yaml",
            "device rules",
        ] {
            assert!(text.contains(named), "{named}: {text}");
        }
    }
    // The switch-on is a first install's (`systemctl preset` when $1 is 1).
    assert!(INSTALL_SCRIPTS.contains("switch on and start"));
    assert!(!UPDATE_SCRIPTS.contains("switch on"));

    // The steps that install no package, and rpm-ostree's, whose way of
    // running the scripts was not read: no note.
    let ostree = Step::Install {
        via: Installer::RpmOstree,
        package: toolkit_base(),
    };
    let enable = Step::EnableRepo {
        repo: RepoId::new("nvidia-container-toolkit").unwrap(),
    };
    for step in [
        Step::add_repo(false),
        Step::add_key(Some(false), true),
        enable,
        ostree,
    ] {
        assert_eq!(package_scripts(&step), None, "{step:?}");
    }
}

/// The note is display only: the hashed plan has no place for it, and the
/// schema stays the one the helper reads.
#[test]
fn the_note_is_not_in_the_hash() {
    let plan = Machine::fedora().plan();
    let hashed = plan.hashed();
    assert_eq!(hashed.schema, PLAN_SCHEMA);
    let encoded = String::from_utf8_lossy(&hashed.encode()).into_owned();
    assert!(!encoded.contains("install scripts"), "{encoded}");
    assert!(plan.steps.iter().any(|s| package_scripts(s).is_some()));
}
