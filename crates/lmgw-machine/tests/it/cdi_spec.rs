//! Spec parsing: nvidia-ctk's YAML, a JSON spec, and what the CDI library
//! refuses.

use std::path::{Path, PathBuf};

use lmgw_machine::cdi::spec::{Format, Spec};

use crate::support::SPEC;

#[test]
fn nvidia_ctks_spec_parses_with_its_three_device_names() {
    let spec = Spec::parse(SPEC.as_bytes(), Format::Yaml).unwrap();
    assert_eq!(spec.kind, "nvidia.com/gpu");
    assert_eq!(spec.cdi_version, "0.7.0");
    assert_eq!(
        spec.qualified_names(),
        vec![
            "nvidia.com/gpu=0",
            "nvidia.com/gpu=GPU-00000000-0000-4000-8000-000000000001",
            "nvidia.com/gpu=all",
        ]
    );
}

#[test]
fn the_named_paths_cover_nodes_hooks_and_mounts() {
    let spec = Spec::parse(SPEC.as_bytes(), Format::Yaml).unwrap();
    let paths = spec.named_paths();
    for p in [
        "/dev/nvidia0",
        "/dev/nvidia-uvm",
        "/dev/dri/renderD128",
        "/usr/bin/nvidia-cdi-hook",
        "/usr/lib64/libnvidia-ml.so.600.20.03",
        "/usr/lib64/libnvidia-egl-wayland.so.1.1.24",
    ] {
        assert!(paths.contains(&PathBuf::from(p)), "{p} missing");
    }
    // Hook arguments name container-side links, not host paths.
    assert!(!paths.iter().any(|p| p.to_string_lossy().contains("::")));
    // Each once.
    assert_eq!(
        paths.len(),
        paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
}

#[test]
fn a_device_node_with_a_host_path_names_the_host_path() {
    let spec = Spec::parse(
        br#"{"cdiVersion":"0.6.0","kind":"example.com/dev","devices":[{"name":"a",
            "containerEdits":{"deviceNodes":[{"path":"/dev/in-container","hostPath":"/dev/on-host"}]}}]}"#,
        Format::Json,
    )
    .unwrap();
    assert_eq!(
        spec.named_paths().into_iter().collect::<Vec<_>>(),
        vec![PathBuf::from("/dev/on-host")]
    );
}

#[test]
fn the_format_is_the_extension() {
    assert_eq!(Format::of(Path::new("/etc/cdi/a.yaml")), Some(Format::Yaml));
    assert_eq!(Format::of(Path::new("/etc/cdi/a.json")), Some(Format::Json));
    assert_eq!(Format::of(Path::new("/etc/cdi/a.yml")), None);
    assert_eq!(Format::of(Path::new("/etc/cdi/.nvidia.yaml.tmp")), None);
    assert!(Spec::parse_file(Path::new("/x/a.txt"), b"").is_err());
}

#[test]
fn a_bad_kind_or_device_name_is_refused() {
    let kind = Spec::parse(
        b"cdiVersion: 0.7.0\nkind: nogpu\ndevices: []\n",
        Format::Yaml,
    );
    assert!(kind.unwrap_err().0.contains("vendor/class"));
    let name = Spec::parse(
        b"cdiVersion: 0.7.0\nkind: nvidia.com/gpu\ndevices:\n  - name: \"a b\"\n",
        Format::Yaml,
    );
    assert!(name.unwrap_err().0.contains("a b"));
    let junk = Spec::parse(b"{{{", Format::Yaml);
    assert!(junk.unwrap_err().0.starts_with("not a CDI spec"));
}
