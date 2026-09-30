//! `sd-server --help` → a queryable flag vocabulary for one image
//! (image-generation design §2.6, §3).
//!
//! The image class stores a row's weights as a JSON map of **canonical keys**
//! (`diffusion_model`, `clip_l`, `vae`) and its runtime/generation flags as a
//! second one (`cfg_scale`, `offload_to_cpu`). Rendering those into argv needs
//! one fact this crate cannot derive: **the exact spelling of the flag**.
//! sd-server mixes separators in a way no mechanical rule reproduces —
//! `--clip_l`, `--clip_g`, `--t5xxl`, `--llm_vision` and `--clip_vision` use
//! underscores while `--diffusion-model`, `--control-net`, `--ip-adapter`,
//! `--lora-model-dir`, `--offload-to-cpu`, `--diffusion-fa` and `--vae-tiling`
//! use hyphens. A `_` → `-` replacement would render `--clip-l`, which the
//! server rejects. So the spelling is looked up in the image's own `--help`,
//! never computed.
//!
//! This module is the llama-side [`crate::llama_caps`]'s sibling and shares its
//! two rules: it is **pure** (it parses text and spawns nothing — running the
//! binary is [`crate::runtime::registry::Registry`]'s job), and the vocabulary
//! is a property of the *image*, so the cache that feeds it is keyed by image
//! reference. It is a separate parser rather than a reuse of `llama_caps`
//! because sd-server's help has a different shape: section headers
//! (`Context Options:`), a wider description column, wrapped continuation
//! lines, and `<string>` / `<int>` / `<float>` placeholders instead of
//! llama.cpp's `FNAME` / `[on|off]` / `{a,b}` vocabulary.
//!
//! # The shape of the help text
//!
//! ```text
//! Context Options:
//!   -m, --model <string>                     path to full model
//!   --diffusion-model <string>               path to the standalone diffusion model
//!   --eager-load                             load all params into the params backend at model-load time
//!                                            instead of lazily on first use (defaults to false)
//! ```
//!
//! An entry starts at column 2 with a `-`; its aliases are comma-separated,
//! an optional `<type>` placeholder follows the last one, and the description
//! begins after the next run of two or more spaces and wraps into deeply
//! indented continuation lines. Section headers and the two banner lines start
//! at column 0 and are skipped.
//!
//! # `takes_value` is what the help says — and where it says nothing, a rule
//!
//! sd-server prints a `<string>`/`<int>`/`<float>` placeholder for most
//! value-taking flags, but not for all of them: `--type`, `--seed`,
//! `--sampling-method`, `--scheduler` and two dozen others take a value with
//! no placeholder at all. For those the description decides, against the
//! marker list in [`describes_a_value`] — `one of [`, `default:`, `(default)`,
//! `comma-separated`, `key=value`, `example`, `path to`, `format `,
//! `can be used`. Measured against the committed `c678dfe` help (157 long
//! flags): the rule classifies 129 as value-taking and 28 as switches, and the
//! single flag it gets wrong is **`--cache-mode`**, whose help states neither a
//! placeholder nor a marker and is therefore reported as a switch.
//!
//! That is affordable because nothing in the renderer depends on it:
//! [`crate::runtime::argv::render_image_args`] decides switch-vs-value from the
//! *row's own JSON value type* (`true` is a bare switch, everything else is
//! `--flag value`), exactly as the stored map means it. `takes_value` is for
//! surfaces that want to say what a flag expects; a wrong answer costs a hint,
//! never an argv.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::Serialize;

/// The committed `sd-server --help` of
/// `ghcr.io/leejet/stable-diffusion.cpp:master-cuda` at commit `c678dfe`
/// (design §12), embedded so that rendering never depends on a probe.
///
/// The *same file* the tests read as a fixture, included from `tests/` rather
/// than copied under `src/`: two copies of a 256-line vocabulary would drift,
/// and the one property that matters here is that what the renderer falls back
/// to and what the tests assert against are the same bytes. (`src/gguf.rs`
/// already includes chat-template fixtures from this directory.)
const EMBEDDED_HELP: &str = include_str!("../tests/fixtures/sdcpp/sd-server-help-c678dfe.txt");

/// A canonical key: the long flag without its dashes, `-` folded to `_`,
/// lowercased. `--diffusion-model` and `--clip_l` both round-trip through it
/// (`diffusion_model`, `clip_l`) — which is exactly why the reverse direction
/// needs [`SdcppCaps::flag_for`] rather than a substitution.
pub fn canonical_key(s: &str) -> String {
    s.trim()
        .trim_start_matches('-')
        .trim()
        .replace('-', "_")
        .to_ascii_lowercase()
}

/// One parsed help entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SdcppFlag {
    /// The exact long flag, dashes and all: `--diffusion-model`, `--clip_l`.
    pub flag: String,
    /// Canonical key of [`Self::flag`].
    pub key: String,
    /// Every other spelling the same entry declares, dashes and all: short
    /// forms (`-m`) and any extra long form.
    pub aliases: Vec<String>,
    /// Whether the flag expects a value — see the module docs for how this is
    /// decided when the help prints no placeholder.
    pub takes_value: bool,
    /// The placeholder the help printed, without the angle brackets
    /// (`string`, `int`, `float`), when it printed one.
    pub value_hint: Option<String>,
    /// The description, continuation lines folded into one line.
    pub description: String,
}

/// What one `sd-server` build accepts, parsed from its `--help`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SdcppCaps {
    /// Entries in the order the help lists them.
    pub flags: Vec<SdcppFlag>,
    /// Canonical key (of the long flag *and* of every alias) → index into
    /// [`Self::flags`]. First registration wins, so a primary long flag is
    /// never shadowed by another entry's alias.
    index: BTreeMap<String, usize>,
}

impl SdcppCaps {
    /// Parse `sd-server --help` output.
    pub fn parse(help: &str) -> Self {
        let mut caps = Self::default();
        for entry in parse_entries(help) {
            caps.push(entry);
        }
        caps
    }

    /// The vocabulary of the build this lmgw shipped with ([`EMBEDDED_HELP`]),
    /// parsed once per process.
    ///
    /// The fallback for every path that cannot reach a real image: the help
    /// probe needs the image pulled *and* the GPU device attached (the binary
    /// links `libcuda.so.1` directly, so even `-h` exits 127 without it —
    /// measured, §12.7). Rendering must not stop for that, so it falls back
    /// here and the start carries a warning instead.
    pub fn embedded() -> Arc<SdcppCaps> {
        static EMBEDDED: OnceLock<Arc<SdcppCaps>> = OnceLock::new();
        EMBEDDED
            .get_or_init(|| Arc::new(SdcppCaps::parse(EMBEDDED_HELP)))
            .clone()
    }

    fn push(&mut self, flag: SdcppFlag) {
        let at = self.flags.len();
        self.index.entry(flag.key.clone()).or_insert(at);
        for alias in &flag.aliases {
            self.index.entry(canonical_key(alias)).or_insert(at);
        }
        self.flags.push(flag);
    }

    /// The entry a canonical key names, by long flag or by alias.
    pub fn get(&self, key: &str) -> Option<&SdcppFlag> {
        let key = canonical_key(key);
        self.index.get(&key).and_then(|i| self.flags.get(*i))
    }

    /// **The spelling lookup**: the exact flag (with dashes) to render for a
    /// canonical key, or `None` when this build has no such flag.
    pub fn flag_for(&self, key: &str) -> Option<&str> {
        self.get(key).map(|f| f.flag.as_str())
    }

    /// The canonical key of a stored key **as this build spells it**: `m`,
    /// `-m`, `--model` and `model` all resolve to `model`, because the help
    /// declares `-m` as an alias of `--model`.
    ///
    /// The guard half of [`Self::flag_for`]: anything that asks "has this flag
    /// been claimed already" has to ask under one spelling, or a row sets
    /// `--listen-ip` a second time by calling it `l`. An unknown key keeps its
    /// own canonical form, so it is still reported by name.
    pub fn resolve_key(&self, key: &str) -> String {
        self.get(key)
            .map(|f| f.key.clone())
            .unwrap_or_else(|| canonical_key(key))
    }

    /// Whether this flag's value is a **path**, which is what decides which of
    /// an image row's two maps it belongs in (design §4: `files` values are
    /// rewritten onto the container's `/models` mount, `args` values are
    /// passed through verbatim).
    ///
    /// Read off the help rather than from a list, for the same reason the
    /// vocabulary itself is: every path flag sd-server has describes itself as
    /// `path to …` or as a `… directory`, and the family sd.cpp adds next
    /// month will too. A `_dir` key counts regardless of what its description
    /// says.
    pub fn is_path_flag(&self, key: &str) -> bool {
        if crate::runtime::image::is_dir_key(&self.resolve_key(key)) {
            return true;
        }
        self.get(key).is_some_and(|f| {
            let d = f.description.to_ascii_lowercase();
            d.contains("path to") || d.contains("directory")
        })
    }

    /// Whether the flag expects a value; `None` for an unknown key.
    pub fn takes_value(&self, key: &str) -> Option<bool> {
        self.get(key).map(|f| f.takes_value)
    }

    pub fn is_known(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Number of long flags this build declares.
    pub fn len(&self) -> usize {
        self.flags.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flags.is_empty()
    }

    /// Every key of an image row that this build does not know, rendered as
    /// one problem each (design §4's pre-flight: "a `files` key or `args` key
    /// outside the image's help vocabulary is a problem naming the key").
    ///
    /// Both maps are checked against the same vocabulary because they end up
    /// in the same argv — the split between them is lmgw's (paths vs
    /// everything else), not sd-server's.
    pub fn validate_keys(
        &self,
        files: &serde_json::Map<String, serde_json::Value>,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (what, map) in [("files", files), ("args", args)] {
            for key in map.keys() {
                if self.is_known(key) {
                    continue;
                }
                let base = format!("{what} key '{key}' is not an sd-server flag in this image");
                out.push(match self.suggest(key) {
                    Some(hit) => format!("{base} (did you mean '{hit}'?)"),
                    None => base,
                });
            }
        }
        out
    }

    /// Closest known key to a mistyped one, by containment rather than edit
    /// distance: a truncated or over-long key (`diffusion_mode`,
    /// `vae_tiling_x`) is the mistake this catches, and pulling a Levenshtein
    /// implementation across the crate for the rest would buy little. `None`
    /// when nothing contains it either way.
    pub fn suggest(&self, key: &str) -> Option<&str> {
        let needle = canonical_key(key);
        if needle.len() < 4 {
            return None;
        }
        self.index
            .iter()
            .filter(|(k, _)| k.contains(&needle) || needle.contains(k.as_str()))
            .filter(|(k, _)| k.len() >= 4)
            .min_by_key(|(k, _)| (k.len().abs_diff(needle.len()), k.len()))
            .and_then(|(_, i)| self.flags.get(*i))
            .map(|f| f.flag.as_str())
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// One entry head plus its continuation lines, before classification.
struct RawEntry {
    flags: Vec<String>,
    placeholder: Option<String>,
    desc: Vec<String>,
}

/// True for an entry head: column 2 exactly, starting with a dash.
///
/// Continuation lines are indented to the description column (43 in this
/// build) and three of them in the `c678dfe` help begin with a dash
/// (`--video-frames > 1`), so the indentation — not the dash — is what
/// separates the two.
fn is_head(line: &str) -> bool {
    line.starts_with("  -") && !line.starts_with("   ")
}

/// True for a continuation line: indented past the head column, non-empty.
fn is_continuation(line: &str) -> bool {
    line.starts_with("   ") && !line.trim().is_empty()
}

fn parse_entries(help: &str) -> Vec<SdcppFlag> {
    let mut raw: Vec<RawEntry> = Vec::new();
    for line in help.lines() {
        if is_head(line) {
            let body = line.trim_start();
            // The head ends at the first run of two or more spaces; a flag
            // list never contains one, and the description column always
            // starts after one.
            let (head, desc) = match find_gap(body) {
                Some((end, start)) => (&body[..end], body[start..].trim()),
                None => (body, ""),
            };
            let mut flags: Vec<String> = head
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            // `<string>` rides on the last alias: `-m, --model <string>`.
            let mut placeholder = None;
            if let Some(last) = flags.last_mut() {
                if let Some((flag, rest)) = last.clone().split_once(' ') {
                    *last = flag.to_string();
                    placeholder = Some(
                        rest.trim()
                            .trim_start_matches('<')
                            .trim_end_matches('>')
                            .to_string(),
                    );
                }
            }
            raw.push(RawEntry {
                flags,
                placeholder,
                desc: if desc.is_empty() {
                    Vec::new()
                } else {
                    vec![desc.to_string()]
                },
            });
        } else if is_continuation(line) {
            if let Some(last) = raw.last_mut() {
                last.desc.push(line.trim().to_string());
            }
        }
        // Anything else (blank lines, the banner, `Context Options:`) ends
        // nothing: the next head starts a new entry on its own.
    }

    raw.into_iter()
        .filter_map(|e| {
            let description = e.desc.join(" ");
            // The primary flag is the first long spelling; a short-only entry
            // (none exist in this build) keeps its short form.
            let long_at = e.flags.iter().position(|f| f.starts_with("--"));
            let primary = e.flags.get(long_at.unwrap_or(0))?.clone();
            let aliases: Vec<String> = e
                .flags
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != long_at && (*i != 0 || long_at.is_some()))
                .map(|(_, f)| f.clone())
                .collect();
            let takes_value = e.placeholder.is_some() || describes_a_value(&description);
            Some(SdcppFlag {
                key: canonical_key(&primary),
                flag: primary,
                aliases,
                takes_value,
                value_hint: e.placeholder,
                description,
            })
        })
        .collect()
}

/// Byte offsets of the first run of two or more spaces: `(run start, text
/// after the run)`.
fn find_gap(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b' ' && b[i + 1] == b' ' {
            let start = i;
            while i < b.len() && b[i] == b' ' {
                i += 1;
            }
            return Some((start, i));
        }
        i += 1;
    }
    None
}

/// Whether a description says its flag expects a value, for the flags whose
/// help prints no `<type>` placeholder. See the module docs for the measured
/// accuracy of this list and its one known miss.
fn describes_a_value(desc: &str) -> bool {
    const MARKERS: &[&str] = &[
        // "one of [euler, euler_a, …]"
        "one of [",
        // "(default: 42…)", "(float, default: 0…)" — note the colon: the
        // switches spell their default "(defaults to false)".
        "default:",
        // "'dynamic' (default) or 'static'"
        "(default)",
        // "comma-separated (e.g., \"14.61,7.8\")"
        "comma-separated",
        // "named cache params (key=value format…)"
        "key=value",
        // "weight type (examples: f32, f16, …)"
        "example",
        // "path to the file containing the prompt"
        "path to",
        // "format [X]x[Y]"
        "format ",
        // "(can be used multiple times)"
        "can be used",
    ];
    MARKERS.iter().any(|m| desc.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Arc<SdcppCaps> {
        SdcppCaps::embedded()
    }

    /// The count is the regression guard: a parser that silently stopped
    /// reading at a section header, or started eating continuation lines as
    /// entries, changes this number long before it changes any single lookup.
    #[test]
    fn the_committed_help_parses_into_157_long_flags() {
        assert_eq!(caps().len(), 157);
    }

    /// The whole reason this module exists: a canonical key maps back to the
    /// image's own spelling, which mixes separators per flag.
    #[test]
    fn a_canonical_key_maps_back_to_the_exact_spelling() {
        let c = caps();
        // Underscore flags — a mechanical `_` → `-` would render `--clip-l`.
        assert_eq!(c.flag_for("clip_l"), Some("--clip_l"));
        assert_eq!(c.flag_for("clip_g"), Some("--clip_g"));
        assert_eq!(c.flag_for("t5xxl"), Some("--t5xxl"));
        assert_eq!(c.flag_for("llm_vision"), Some("--llm_vision"));
        assert_eq!(c.flag_for("clip_vision"), Some("--clip_vision"));
        // Hyphen flags — a mechanical pass-through would render
        // `--diffusion_model`.
        assert_eq!(c.flag_for("diffusion_model"), Some("--diffusion-model"));
        assert_eq!(c.flag_for("control_net"), Some("--control-net"));
        assert_eq!(c.flag_for("ip_adapter"), Some("--ip-adapter"));
        assert_eq!(c.flag_for("lora_model_dir"), Some("--lora-model-dir"));
        assert_eq!(
            c.flag_for("hires_upscalers_dir"),
            Some("--hires-upscalers-dir")
        );
        assert_eq!(c.flag_for("offload_to_cpu"), Some("--offload-to-cpu"));
        assert_eq!(c.flag_for("diffusion_fa"), Some("--diffusion-fa"));
        assert_eq!(c.flag_for("vae_tiling"), Some("--vae-tiling"));
        assert_eq!(
            c.flag_for("high_noise_diffusion_model"),
            Some("--high-noise-diffusion-model")
        );
        // Plain ones, and the dashed spelling of a key accepted too.
        assert_eq!(c.flag_for("vae"), Some("--vae"));
        assert_eq!(c.flag_for("llm"), Some("--llm"));
        assert_eq!(c.flag_for("--diffusion-model"), Some("--diffusion-model"));
    }

    /// Short forms resolve to the same entry, so a row that spells a key `m`
    /// still renders `--model` rather than being rejected as unknown.
    #[test]
    fn short_aliases_resolve_to_their_long_flag() {
        let c = caps();
        assert_eq!(c.flag_for("m"), Some("--model"));
        assert_eq!(c.flag_for("t"), Some("--threads"));
        assert_eq!(c.flag_for("w"), Some("--width"));
        assert_eq!(c.flag_for("h"), Some("--help"), "-h is help, not height");
        assert_eq!(c.flag_for("r"), Some("--ref-image"));
    }

    #[test]
    fn takes_value_follows_the_placeholder_when_there_is_one() {
        let c = caps();
        assert_eq!(c.takes_value("diffusion_model"), Some(true));
        assert_eq!(c.takes_value("vae"), Some(true));
        assert_eq!(c.takes_value("steps"), Some(true));
        assert_eq!(c.takes_value("cfg_scale"), Some(true));
        assert_eq!(
            c.get("model").unwrap().value_hint.as_deref(),
            Some("string")
        );
        assert_eq!(c.get("steps").unwrap().value_hint.as_deref(), Some("int"));
        assert_eq!(
            c.get("cfg_scale").unwrap().value_hint.as_deref(),
            Some("float")
        );
    }

    /// The flags with no placeholder at all: the description rule has to carry
    /// both directions, or half of `Default Generation Options` would be
    /// mistaken for switches.
    #[test]
    fn takes_value_falls_back_to_the_description_when_there_is_no_placeholder() {
        let c = caps();
        for key in [
            "type",
            "seed",
            "sampling_method",
            "scheduler",
            "rng",
            "prediction",
            "lora_apply_mode",
            "auto_fit",
            "log_level",
            "linear_scale",
            "sigmas",
            "vae_tile_size",
            "prompt_file",
        ] {
            assert_eq!(c.takes_value(key), Some(true), "{key} takes a value");
        }
        for key in [
            "eager_load",
            "offload_to_cpu",
            "diffusion_fa",
            "fa",
            "vae_tiling",
            "hires",
            "mmap",
            "sage_attn",
            "circular",
            "list_devices",
            "help",
        ] {
            assert_eq!(c.takes_value(key), Some(false), "{key} is a switch");
        }
        // The one the rule gets wrong, asserted so the day the help gains a
        // marker for it is a visible change rather than a silent one.
        assert_eq!(
            c.takes_value("cache_mode"),
            Some(false),
            "known miss: --cache-mode's help states neither a placeholder nor a value marker"
        );
    }

    #[test]
    fn an_unknown_key_has_no_flag_and_no_arity() {
        let c = caps();
        assert_eq!(c.flag_for("clip-l-encoder"), None);
        assert_eq!(c.takes_value("not_a_flag_at_all"), None);
        assert!(!c.is_known("difusion_model"));
    }

    #[test]
    fn validate_keys_names_every_unknown_key_and_suggests_a_near_miss() {
        let c = caps();
        let files = serde_json::json!({
            "diffusion_model": "a.gguf",
            "vae": "b.safetensors",
            "clip-l": "c.safetensors",
            "diffusion_mode": "typo.gguf"
        });
        let args = serde_json::json!({"steps": 8, "offload_to_cpu": true, "nonsense_flag": 1});
        let problems = c.validate_keys(files.as_object().unwrap(), args.as_object().unwrap());
        // `clip-l` canonicalizes to `clip_l`, which is a real flag.
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("files key 'diffusion_mode'")
                && problems[0].contains("--diffusion-model"),
            "{problems:?}"
        );
        assert!(
            problems[1].contains("args key 'nonsense_flag'"),
            "{problems:?}"
        );
    }

    /// Continuation lines are description, never entries — and the wrapped
    /// text has to survive intact, because `takes_value` reads it.
    #[test]
    fn wrapped_descriptions_are_folded_and_never_parsed_as_flags() {
        let c = caps();
        assert!(c.flag_for("video_frames_gt_1").is_none());
        let auto_fit = c.get("auto_fit").unwrap();
        assert!(
            auto_fit
                .description
                .contains("automatic graph segmentation"),
            "the last continuation line is missing: {:?}",
            auto_fit.description
        );
    }

    #[test]
    fn parsing_a_hand_written_help_needs_no_container() {
        let caps = SdcppCaps::parse(
            "Usage: /sd.cpp/bin/sd-server [options]\n\
             \n\
             Context Options:\n  \
               -m, --model <string>      path to full model\n  \
               --brand-new-flag          a flag lmgw has never heard of (default: 3)\n  \
               --brand-new-switch        turns something on\n",
        );
        assert_eq!(caps.len(), 3);
        assert_eq!(caps.flag_for("brand_new_flag"), Some("--brand-new-flag"));
        assert_eq!(caps.takes_value("brand_new_flag"), Some(true));
        assert_eq!(caps.takes_value("brand_new_switch"), Some(false));
    }
}
