//! audio.cpp's upstream model-spec catalog: browse families/packages and
//! install them from Hugging Face, the way the project's own Gradio WebUI
//! does.
//!
//! The engine side of audio.cpp — `server.json` rendering and the container
//! it is mounted into — lives in [`crate::runtime::audio`] and
//! [`crate::runtime::registry`] since per-model containers (§3.6): there is
//! no shared audiocpp_server process left for this module to own.

mod carry;
pub mod charset;
pub mod cues;
pub mod engine_errors;
pub mod families;
pub mod files;
pub mod language;
pub mod pins;
pub mod preflight;
pub mod profile;
pub mod published;
pub mod rates;
pub mod shape;
pub mod tags;
pub mod transcript;
pub mod variant;
pub mod voices;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Model-spec catalog (audio.cpp's `model_specs/*.json`)
// ---------------------------------------------------------------------------

/// GitHub repo holding the canonical spec catalog.
pub const SPEC_REPO: &str = "0xShug0/audio.cpp";
pub const SPEC_DIR: &str = "model_specs";
const SPEC_REF: &str = "main";
/// Listing host (git-trees API) and raw-file host of the spec catalog.
pub const SPEC_API_BASE: &str = "https://api.github.com";
pub const SPEC_RAW_BASE: &str = "https://raw.githubusercontent.com";

/// Effective (listing, raw) hosts. `LMGW_AUDIO_CATALOG_ENDPOINT` replaces
/// *both* with one origin — the hook tests and dev runs point at a local mock
/// catalog, surfaced as an env var rather than a hidden constant swap (same
/// deal as `hf::hf_base` and `update::manifest_url`).
pub fn spec_bases() -> (String, String) {
    match std::env::var("LMGW_AUDIO_CATALOG_ENDPOINT") {
        Ok(base) if !base.trim().is_empty() => {
            let base = base.trim().trim_end_matches('/').to_string();
            (base.clone(), base)
        }
        _ => (SPEC_API_BASE.to_string(), SPEC_RAW_BASE.to_string()),
    }
}

/// Where a package's files come from — or why they cannot be fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecDownload {
    /// `huggingface_snapshot`, or `unsupported` for a family whose weights
    /// upstream may not redistribute.
    pub kind: String,
    /// Empty when `kind` is `unsupported` — see [`Self::reason`].
    pub repo: String,
    /// Why there is nothing to download, in upstream's own words (a licence
    /// that forbids redistribution, a build that is not published yet). Shown
    /// instead of a bare "no download source", because the answer to it is a
    /// local conversion, not a wait.
    #[serde(default)]
    pub reason: String,
    /// Branch/tag/commit the spec pins, when it pins one (`main` for most).
    #[serde(default)]
    pub revision: Option<String>,
    /// The repo needs an accepted licence and a token. Queueing a gated
    /// download without one produces a pile of 401s in the download list, so
    /// this is checked before anything is queued.
    #[serde(default)]
    pub gated: bool,
}

/// One option a family accepts, as the spec declares it: the typed schema
/// audio.cpp added to `model_specs` (`options.request|load|session`).
///
/// This is what turns "load_options is a JSON object, good luck" into a list
/// of names with types, defaults and ranges — the same information the
/// engine validates against, rather than a second copy maintained by hand
/// here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpecOption {
    pub name: String,
    /// `string` | `int` | `float` | `bool` | `enum` | `path` | `audio_path` |
    /// `string_list` | … — the spec's vocabulary, passed through as written.
    pub kind: String,
    pub description: String,
    pub required: bool,
    /// The engine's default when the request leaves it out, when the spec
    /// states one.
    pub default: Option<Value>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Accepted values of an `enum` option — either written out in the spec
    /// or named by a shared `preset` ([`preset_values`]).
    pub values: Vec<String>,
}

/// The three option groups of a family: where a value belongs decides where
/// it goes in `server.json` (`load_options` / `session_options`) or in the
/// request body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpecOptions {
    pub request: Vec<SpecOption>,
    pub load: Vec<SpecOption>,
    pub session: Vec<SpecOption>,
}

impl SpecOptions {
    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.load.is_empty() && self.session.is_empty()
    }
}

/// audio.cpp's shared enum presets (`src/framework/model_spec/options.cpp`),
/// which a spec option names instead of listing the values again.
///
/// Copied rather than resolved: the table is six rows that change about as
/// often as the file format, and the alternative is fetching a C++ source
/// file to render a dropdown. An unknown preset name yields no values, which
/// reads in the UI as "the spec did not say" — never as a wrong list.
fn preset_values(name: &str) -> Vec<String> {
    let vals: &[&str] = match name {
        "best_of_n_language" => &["auto", "en", "ja"],
        "perf_mode_flash_attention" => &["off", "flash_attention"],
        "text_chunk_mode_full" => &["word_budget", "tag_aware", "japanese", "endline"],
        "weight_type_codec_q8" => &["native", "f32", "f16", "q8_0"],
        "weight_type_conv" => &["native", "f32", "f16"],
        "weight_type_full" => &["native", "f32", "f16", "bf16", "q8_0"],
        _ => &[],
    };
    vals.iter().map(|v| v.to_string()).collect()
}

/// One installable package of a model family (a GGUF/safetensors file set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecPackage {
    pub id: String,
    pub display_name: String,
    /// audio.cpp's recommended package for the family.
    pub default: bool,
    pub format: String,
    pub precision: String,
    /// Install dir under the models root in audio.cpp's own layout —
    /// informational here: downloads keep this gateway's HF `owner/repo/file`
    /// layout, and the suggested model path derives from the file list.
    pub target_directory: String,
    pub files: Vec<String>,
    /// Prefix the spec strips from `files` when it lays the package out in
    /// audio.cpp's own tree. Informational here for the same reason
    /// `target_directory` is: downloads keep this gateway's HF layout.
    #[serde(default)]
    pub strip_prefix: String,
    /// What this package is, when the spec says (a size, a precision trade).
    #[serde(default)]
    pub description: String,
    /// Per-package download override (wins over the family default).
    pub download: Option<SpecDownload>,
}

/// One model family from `model_specs/<family>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub family: String,
    /// The spec file this came from (`model_specs/<stem>.json`), so a
    /// refresh in which that file fails can keep the family as it was.
    /// Empty in a snapshot saved before it was recorded, and for a spec
    /// parsed from anywhere else.
    #[serde(default)]
    pub source: String,
    pub display_name: String,
    pub description: String,
    pub category: String,
    pub tasks: Vec<String>,
    pub modes: Vec<String>,
    pub languages: Vec<String>,
    /// `supported` | `community` | `experimental` | `testing` | `wip` — how
    /// finished upstream considers this family. Worth showing before a 20 GiB
    /// download, and absent from the catalog lmgw parsed before.
    #[serde(default)]
    pub status: String,
    /// `ui.tags` — the short labels the project's own WebUI shows.
    #[serde(default)]
    pub tags: Vec<String>,
    /// `ui.docs` — paths into the audio.cpp repo documenting the family.
    #[serde(default)]
    pub docs: Vec<String>,
    /// `ui.summary`, where a family has one (a line beside the description).
    #[serde(default)]
    pub summary: String,
    /// `ui.builtin_voices` / `ui.default_voice` — voices the family ships, so
    /// a TTS row can be configured without first starting the container to
    /// ask it.
    #[serde(default)]
    pub builtin_voices: Vec<String>,
    #[serde(default)]
    pub default_voice: Option<String>,
    /// `capabilities` — per-task capability tags (`{"clone":
    /// ["speaker_reference"]}`), passed through as the spec writes them.
    #[serde(default)]
    pub capabilities: serde_json::Map<String, Value>,
    /// The typed option schema (`options.request|load|session`).
    #[serde(default)]
    pub options: SpecOptions,
    /// `ui.recommended_package` — wins over `packages[].default` when set.
    pub recommended_package: Option<String>,
    /// `package_defaults.download` — the family-wide HF repo.
    pub default_download: Option<SpecDownload>,
    pub packages: Vec<SpecPackage>,
}

impl ModelSpec {
    /// The package audio.cpp installs by default (WebUI behavior: prefer the
    /// ui recommendation, then the `default` flag, then the first package).
    pub fn recommended(&self) -> Option<&SpecPackage> {
        self.recommended_package
            .as_deref()
            .and_then(|id| self.packages.iter().find(|p| p.id == id))
            .or_else(|| self.packages.iter().find(|p| p.default))
            .or_else(|| self.packages.first())
    }

    /// The download a package resolves to: its own, else the family default.
    pub fn package_download<'a>(&'a self, pkg: &'a SpecPackage) -> Option<&'a SpecDownload> {
        pkg.download.as_ref().or(self.default_download.as_ref())
    }

    /// HF repo a package's files download from — `None` when the spec names
    /// none, which since `kind: "unsupported"` may also mean "deliberately
    /// not published"; [`Self::package_reason`] has that half.
    pub fn package_repo<'a>(&'a self, pkg: &'a SpecPackage) -> Option<&'a str> {
        self.package_download(pkg)
            .map(|d| d.repo.as_str())
            .filter(|r| !r.is_empty())
    }

    /// Why a package cannot be installed from here, when the spec says.
    pub fn package_reason<'a>(&'a self, pkg: &'a SpecPackage) -> Option<&'a str> {
        self.package_download(pkg)
            .map(|d| d.reason.as_str())
            .filter(|r| !r.is_empty())
    }
}

/// Lenient spec parse: unknown/missing fields fall back to defaults so the
/// catalog keeps working when audio.cpp adds spec keys.
pub fn parse_spec(v: &Value) -> ModelSpec {
    let strings = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let download = |d: Option<&Value>| -> Option<SpecDownload> {
        let d = d?;
        let repo = d.get("repo").and_then(Value::as_str).unwrap_or_default();
        let reason = d.get("reason").and_then(Value::as_str).unwrap_or_default();
        // A block with neither is nothing at all; one with only a reason is a
        // family whose weights upstream may not redistribute, and saying so
        // is the whole point of keeping it.
        if repo.is_empty() && reason.is_empty() {
            return None;
        }
        Some(SpecDownload {
            kind: d
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("huggingface_snapshot")
                .to_string(),
            repo: repo.to_string(),
            reason: reason.to_string(),
            revision: d
                .get("revision")
                .and_then(Value::as_str)
                .map(str::to_string),
            gated: d.get("gated").and_then(Value::as_bool).unwrap_or(false),
        })
    };
    let options = |group: &str| -> Vec<SpecOption> {
        v.get("options")
            .and_then(|o| o.get(group))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|o| {
                        let name = o.get("name").and_then(Value::as_str)?;
                        // An enum either lists its values or names a shared
                        // preset; both arrive here as one list.
                        let mut values: Vec<String> = o
                            .get("values")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(String::from)
                                    .collect()
                            })
                            .unwrap_or_default();
                        if values.is_empty() {
                            if let Some(preset) = o.get("preset").and_then(Value::as_str) {
                                values = preset_values(preset);
                            }
                        }
                        Some(SpecOption {
                            name: name.to_string(),
                            kind: o
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            description: o
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            required: o.get("required").and_then(Value::as_bool).unwrap_or(false),
                            default: o.get("default").cloned(),
                            min: o.get("min").and_then(Value::as_f64),
                            max: o.get("max").and_then(Value::as_f64),
                            values,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let ui_strings = |key: &str| -> Vec<String> {
        v.get("ui")
            .and_then(|u| u.get(key))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let ui_string = |key: &str| -> Option<String> {
        v.get("ui")
            .and_then(|u| u.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let packages = v
        .get("packages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|p| SpecPackage {
                    id: p
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    display_name: p
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    default: p.get("default").and_then(Value::as_bool).unwrap_or(false),
                    format: p
                        .get("format")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    precision: p
                        .get("precision")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    target_directory: p
                        .get("target_directory")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    files: p
                        .get("files")
                        .and_then(Value::as_array)
                        .map(|f| {
                            f.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    strip_prefix: p
                        .get("strip_prefix")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: p
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    download: download(p.get("download")),
                })
                .filter(|p: &SpecPackage| !p.id.is_empty() && !p.files.is_empty())
                .collect()
        })
        .unwrap_or_default();
    ModelSpec {
        source: String::new(),
        family: v
            .get("family")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        display_name: v
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        category: v
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tasks: strings("tasks"),
        modes: strings("modes"),
        languages: strings("languages"),
        status: v
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tags: ui_strings("tags"),
        docs: ui_strings("docs"),
        summary: ui_string("summary").unwrap_or_default(),
        builtin_voices: ui_strings("builtin_voices"),
        default_voice: ui_string("default_voice"),
        capabilities: v
            .get("capabilities")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        options: SpecOptions {
            request: options("request"),
            load: options("load"),
            session: options("session"),
        },
        recommended_package: v
            .get("ui")
            .and_then(|u| u.get("recommended_package"))
            .and_then(Value::as_str)
            .map(String::from),
        default_download: download(v.get("package_defaults").and_then(|d| d.get("download"))),
        packages,
    }
}

/// A fetched snapshot of the whole catalog, persisted in the kv store so the
/// Audio tab renders without a GitHub round-trip on every page load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    /// RFC3339 fetch time (shown on the page).
    pub fetched_at: String,
    pub specs: Vec<ModelSpec>,
    /// Which spec-referenced files each package repo publishes, listed at
    /// refresh ([`published`]). Empty in a snapshot from before listings
    /// existed: availability not checked yet.
    #[serde(default)]
    pub listings: std::collections::BTreeMap<String, published::RepoListing>,
    /// What the refresh could not do — a spec file that did not load, a repo
    /// that could not be listed. Shown until the next refresh replaces them.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Fetch the live catalog: list `model_specs/*.json` via the git-trees API,
/// then pull each spec from the raw host (not API-rate-limited). A spec file
/// that fails while others load is a warning on the snapshot, not dropped
/// unsaid — and its family is kept as `previous` (the snapshot this one
/// replaces) had it, because a flaky link must not hide installed or served
/// families until a later refresh happens to load them.
pub async fn fetch_catalog(
    http: &reqwest::Client,
    previous: Option<&CatalogSnapshot>,
) -> Result<CatalogSnapshot, String> {
    let (api_base, raw_base) = spec_bases();
    let trees_url = format!("{api_base}/repos/{SPEC_REPO}/git/trees/{SPEC_REF}?recursive=1");
    let resp = http
        .get(&trees_url)
        // api.github.com 403s requests without a User-Agent.
        .header("User-Agent", concat!("lmgw/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("listing audio.cpp specs: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("listing audio.cpp specs: HTTP {}", resp.status()));
    }
    let tree: Value = resp
        .json()
        .await
        .map_err(|e| format!("listing audio.cpp specs: {e}"))?;
    let mut paths: Vec<String> = tree
        .get("tree")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|e| e.get("path").and_then(Value::as_str))
                .filter(|p| p.starts_with(&format!("{SPEC_DIR}/")) && p.ends_with(".json"))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    paths.sort();

    // `(path, why)` for a file that did not load.
    let specs: Vec<Result<ModelSpec, (String, String)>> =
        futures::stream::iter(paths.into_iter().map(|path| {
            let http = http.clone();
            let raw_base = raw_base.clone();
            async move {
                let url = format!("{raw_base}/{SPEC_REPO}/{SPEC_REF}/{path}");
                let failed = |why: String| (path.clone(), why);
                let resp = http
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(30))
                    .send()
                    .await
                    .map_err(|e| failed(e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(failed(format!("HTTP {}", resp.status())));
                }
                let v: Value = resp.json().await.map_err(|e| failed(e.to_string()))?;
                let mut spec = parse_spec(&v);
                spec.source = path.clone();
                Ok(spec)
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut out = Vec::with_capacity(specs.len());
    let mut failed = Vec::new();
    for s in specs {
        match s {
            Ok(spec) if !spec.family.is_empty() => out.push(spec),
            Ok(_) => {}
            Err(f) => failed.push(f),
        }
    }
    failed.sort();
    if out.is_empty() {
        let errors: Vec<String> = failed.iter().map(|(p, e)| format!("{p}: {e}")).collect();
        return Err(format!(
            "no audio.cpp specs fetched{}",
            if errors.is_empty() {
                String::new()
            } else {
                format!(": {}", errors.join("; "))
            }
        ));
    }
    let warnings = carry::carry_failed(&mut out, &failed, previous);
    out.sort_by(|a, b| a.family.cmp(&b.family));
    Ok(CatalogSnapshot {
        fetched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        specs: out,
        listings: Default::default(),
        warnings,
    })
}
