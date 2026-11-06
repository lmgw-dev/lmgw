//! System CDI resolution (machine-check design §6, "System CDI resolution"):
//! the later dir wins, a same-dir conflict drops the name even with a later
//! dir defining it, JSON specs, a spec without `all`, and containers.conf's
//! `cdi_spec_dirs`.

use std::path::PathBuf;

use lmgw_machine::cdi::system::{
    cdi_dirs_in, configured_spec_dirs, containers_conf_files, resolve, DEFAULT_SPEC_DIRS,
};
use lmgw_machine::fake::FakeFacts;

use crate::support::SPEC;

fn default_dirs() -> Vec<PathBuf> {
    DEFAULT_SPEC_DIRS.iter().map(PathBuf::from).collect()
}

/// A spec of `kind` defining `names`, as YAML.
fn small_spec(kind: &str, names: &[&str]) -> String {
    let mut s = format!("cdiVersion: 0.7.0\nkind: {kind}\ndevices:\n");
    for n in names {
        s.push_str(&format!(
            "  - name: \"{n}\"\n    containerEdits:\n      deviceNodes:\n        - path: /dev/x{n}\n"
        ));
    }
    s
}

#[test]
fn a_later_dir_wins_over_an_earlier_one() {
    let f = FakeFacts::new();
    f.file("/etc/cdi/nvidia.yaml", SPEC)
        .file("/var/run/cdi/nvidia.yaml", SPEC);
    let res = resolve(&f, &default_dirs());
    assert!(res.conflicts.is_empty(), "{:?}", res.conflicts);
    assert_eq!(
        res.devices.get("nvidia.com/gpu=all"),
        Some(&PathBuf::from("/var/run/cdi/nvidia.yaml"))
    );
    assert_eq!(res.devices.len(), 3);
}

#[test]
fn two_files_in_one_dir_are_a_conflict_and_the_name_is_dropped() {
    let f = FakeFacts::new();
    f.file("/var/run/cdi/nvidia.yaml", SPEC)
        .file("/var/run/cdi/nvidia-copy.yaml", SPEC);
    let res = resolve(&f, &default_dirs());
    assert!(res.devices.is_empty(), "{:?}", res.devices);
    assert_eq!(
        res.conflicts.get("nvidia.com/gpu=all").unwrap(),
        &vec![
            PathBuf::from("/var/run/cdi/nvidia-copy.yaml"),
            PathBuf::from("/var/run/cdi/nvidia.yaml"),
        ]
    );
}

#[test]
fn a_same_dir_conflict_in_etc_cdi_drops_the_name_even_though_var_run_cdi_defines_it() {
    // Pinning what cache.go's refresh does: the conflict set is purged after
    // every dir was read.
    let f = FakeFacts::new();
    f.file("/etc/cdi/a.yaml", SPEC)
        .file("/etc/cdi/b.yaml", SPEC)
        .file("/var/run/cdi/nvidia.yaml", SPEC);
    let res = resolve(&f, &default_dirs());
    assert!(!res.devices.contains_key("nvidia.com/gpu=all"));
    assert!(res.conflicts.contains_key("nvidia.com/gpu=all"));
}

#[test]
fn json_specs_count_and_other_files_and_subdirs_do_not() {
    let f = FakeFacts::new();
    f.file(
        "/etc/cdi/vendor.json",
        r#"{"cdiVersion":"0.6.0","kind":"vendor.example/fpga","devices":[{"name":"0"}]}"#,
    )
    .file("/etc/cdi/notes.txt", "kind: x/y")
    .file(
        "/etc/cdi/sub/nested.yaml",
        small_spec("vendor.example/nested", &["0"]),
    )
    // What `nvidia-ctk --output=<dir>/.nvidia.yaml.tmp` really writes: a
    // `.yaml` the library reads, dot or not.
    .file(
        "/var/run/cdi/.nvidia.yaml.tmp.yaml",
        small_spec("nvidia.com/gpu", &["all"]),
    );
    let res = resolve(&f, &default_dirs());
    assert_eq!(
        res.devices.keys().cloned().collect::<Vec<_>>(),
        vec!["nvidia.com/gpu=all", "vendor.example/fpga=0"]
    );
}

#[test]
fn a_spec_without_all_resolves_only_its_own_names() {
    let f = FakeFacts::new();
    f.file("/etc/cdi/one.yaml", small_spec("nvidia.com/gpu", &["0"]));
    let res = resolve(&f, &default_dirs());
    assert!(res.devices.contains_key("nvidia.com/gpu=0"));
    assert!(!res.devices.contains_key("nvidia.com/gpu=all"));
}

#[test]
fn a_broken_spec_is_an_error_and_defines_nothing() {
    let f = FakeFacts::new();
    f.file("/etc/cdi/broken.yaml", "kind: [")
        .file("/etc/cdi/stamp.json", r#"{"schema":1}"#);
    let res = resolve(&f, &default_dirs());
    assert!(res.devices.is_empty());
    assert_eq!(res.errors.len(), 2, "{:?}", res.errors);
}

#[test]
fn podmans_default_dirs_stand_without_containers_conf() {
    let f = FakeFacts::new();
    f.set_env("HOME", "/home/someone");
    let dirs = configured_spec_dirs(&f);
    assert_eq!(dirs.dirs, default_dirs());
    assert_eq!(dirs.set_by, None);
}

#[test]
fn a_cdi_spec_dirs_override_replaces_and_a_later_file_wins() {
    let f = FakeFacts::new();
    f.set_env("HOME", "/home/someone")
        .file(
            "/usr/share/containers/containers.conf",
            "[engine]\ncdi_spec_dirs = [\"/usr/share/cdi\"]\n",
        )
        .file(
            "/etc/containers/containers.conf.d/50-cdi.conf",
            "[engine]\ncdi_spec_dirs = [\"/opt/cdi\", \"/etc/cdi\"]\n",
        )
        .file(
            "/home/someone/.config/containers/containers.conf",
            "[network]\n",
        );
    let dirs = configured_spec_dirs(&f);
    assert_eq!(
        dirs.dirs,
        vec![PathBuf::from("/opt/cdi"), PathBuf::from("/etc/cdi")]
    );
    assert_eq!(
        dirs.set_by,
        Some(PathBuf::from(
            "/etc/containers/containers.conf.d/50-cdi.conf"
        ))
    );
}

#[test]
fn append_adds_to_the_list_for_this_and_later_files() {
    let f = FakeFacts::new();
    f.set_env("XDG_CONFIG_HOME", "/home/someone/cfg")
        .file(
            "/etc/containers/containers.conf",
            "[engine]\ncdi_spec_dirs = [\"/opt/a\", {append = true}]\n",
        )
        .file(
            "/home/someone/cfg/containers/containers.conf",
            "[engine]\ncdi_spec_dirs = [\"/opt/b\"]\n",
        );
    assert_eq!(
        configured_spec_dirs(&f).dirs,
        ["/etc/cdi", "/var/run/cdi", "/opt/a", "/opt/b"]
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    );
}

#[test]
fn containers_conf_alone_replaces_every_file_and_the_override_comes_last() {
    let f = FakeFacts::new();
    f.set_env("CONTAINERS_CONF", "/tmp/only.conf")
        .set_env("CONTAINERS_CONF_OVERRIDE", "/tmp/last.conf")
        .file(
            "/etc/containers/containers.conf",
            "[engine]\ncdi_spec_dirs = [\"/ignored\"]\n",
        )
        .file("/tmp/only.conf", "[engine]\ncdi_spec_dirs = [\"/only\"]\n")
        .file("/tmp/last.conf", "[engine]\ncdi_spec_dirs = [\"/last\"]\n");
    assert_eq!(
        containers_conf_files(&f),
        vec![
            PathBuf::from("/tmp/only.conf"),
            PathBuf::from("/tmp/last.conf")
        ]
    );
    assert_eq!(configured_spec_dirs(&f).dirs, vec![PathBuf::from("/last")]);
}

#[test]
fn the_rootless_drop_ins_count_for_this_uid_only() {
    let f = FakeFacts::new();
    f.set_uid(1000)
        .file(
            "/etc/containers/containers.rootless.d/1000/cdi.conf",
            "[engine]\ncdi_spec_dirs = [\"/mine\"]\n",
        )
        .file(
            "/etc/containers/containers.rootless.d/1001/cdi.conf",
            "[engine]\ncdi_spec_dirs = [\"/theirs\"]\n",
        );
    assert_eq!(configured_spec_dirs(&f).dirs, vec![PathBuf::from("/mine")]);
}

#[test]
fn a_broken_containers_conf_is_named_and_skipped() {
    let f = FakeFacts::new();
    f.file("/etc/containers/containers.conf", "[engine\n");
    let dirs = configured_spec_dirs(&f);
    assert_eq!(dirs.dirs, default_dirs());
    assert_eq!(dirs.errors.len(), 1);
    assert!(cdi_dirs_in("[engine]\ncdi_spec_dirs = \"/not-an-array\"\n").is_err());
    assert_eq!(cdi_dirs_in("[engine]\n").unwrap(), None);
}
