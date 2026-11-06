//! One CDI spec file: its kind, its devices and the host paths its edits
//! name.
//!
//! Only the fields lmgw reads are modelled; everything else in a spec (device
//! numbers, file modes, hook arguments, environment) is skipped. A spec is
//! YAML or JSON by its extension, as the CDI library decides.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A spec file's contents, as far as lmgw reads them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub cdi_version: String,
    /// `vendor/class`, e.g. `nvidia.com/gpu`.
    pub kind: String,
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Edits applied with any device of this spec.
    #[serde(default)]
    pub container_edits: Option<Edits>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub name: String,
    #[serde(default)]
    pub container_edits: Option<Edits>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edits {
    #[serde(default)]
    pub device_nodes: Vec<DeviceNode>,
    #[serde(default)]
    pub hooks: Vec<Hook>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceNode {
    /// The path in the container, and on the host unless `host_path` says
    /// otherwise.
    pub path: String,
    #[serde(default)]
    pub host_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    #[serde(default)]
    pub hook_name: String,
    /// The program run on the host.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub host_path: String,
    #[serde(default)]
    pub container_path: String,
}

/// Why a spec could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(pub String);

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpecError {}

/// How a spec file is encoded, by its extension; `None` for a file the CDI
/// library does not read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Json,
}

impl Format {
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("yaml") => Some(Self::Yaml),
            Some("json") => Some(Self::Json),
            _ => None,
        }
    }
}

/// The YAML reader's options: no budget and no alias limits. A spec is a
/// file root or lmgw itself wrote, never input from someone else, so the
/// parser's defensive caps would only be a bound nobody chose on a large
/// machine's spec.
fn yaml_options() -> serde_saphyr::Options {
    let mut aliases = serde_saphyr::options::AliasLimits::default();
    aliases.max_total_replayed_events = usize::MAX;
    aliases.max_replay_stack_depth = usize::MAX;
    aliases.max_alias_expansions_per_anchor = usize::MAX;
    let mut o = serde_saphyr::Options::default();
    o.budget = None;
    o.alias_limits = aliases;
    o
}

/// Is `s` a valid CDI device name: letters, digits, `_`, `-`, `.` and `:`.
fn valid_device_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Is `s` a `vendor/class` kind.
fn valid_kind(s: &str) -> bool {
    match s.split_once('/') {
        Some((vendor, class)) => {
            !vendor.is_empty()
                && !class.is_empty()
                && !class.contains('/')
                && vendor
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                && class
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        }
        None => false,
    }
}

impl Spec {
    /// Parse a spec in `format` and check its kind and device names the way
    /// the CDI library refuses a spec.
    pub fn parse(bytes: &[u8], format: Format) -> Result<Self, SpecError> {
        let spec: Spec = match format {
            Format::Yaml => serde_saphyr::from_slice_with_options(bytes, yaml_options())
                .map_err(|e| SpecError(format!("not a CDI spec: {e}")))?,
            Format::Json => serde_json::from_slice(bytes)
                .map_err(|e| SpecError(format!("not a CDI spec: {e}")))?,
        };
        if !valid_kind(&spec.kind) {
            return Err(SpecError(format!(
                "its kind '{}' is not of the form vendor/class",
                spec.kind
            )));
        }
        if let Some(bad) = spec.devices.iter().find(|d| !valid_device_name(&d.name)) {
            return Err(SpecError(format!(
                "its device name '{}' is not a valid CDI device name",
                bad.name
            )));
        }
        Ok(spec)
    }

    /// Parse the spec at `path`, by its extension.
    pub fn parse_file(path: &Path, bytes: &[u8]) -> Result<Self, SpecError> {
        let format = Format::of(path).ok_or_else(|| {
            SpecError(format!(
                "{} is neither .yaml nor .json, so no CDI reader reads it",
                path.display()
            ))
        })?;
        Self::parse(bytes, format)
    }

    /// `kind=name` for every device, as `--device` names them.
    pub fn qualified_names(&self) -> Vec<String> {
        self.devices
            .iter()
            .map(|d| format!("{}={}", self.kind, d.name))
            .collect()
    }

    /// Every host path the spec's edits name — device nodes (their host path,
    /// else their path), hook programs and mount sources — each once, sorted.
    /// Each has to exist for a container that uses the spec to start.
    pub fn named_paths(&self) -> BTreeSet<PathBuf> {
        let mut out = BTreeSet::new();
        let edits = self.container_edits.iter().chain(
            self.devices
                .iter()
                .filter_map(|d| d.container_edits.as_ref()),
        );
        for e in edits {
            for n in &e.device_nodes {
                out.insert(PathBuf::from(n.host_path.as_deref().unwrap_or(&n.path)));
            }
            for h in &e.hooks {
                out.insert(PathBuf::from(&h.path));
            }
            for m in &e.mounts {
                out.insert(PathBuf::from(&m.host_path));
            }
        }
        out
    }
}
