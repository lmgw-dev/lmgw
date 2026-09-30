//! `llama-server --help` → a queryable flag vocabulary for one image.
//!
//! Local models are rows that render into a llama-server command line (see
//! [`crate::runtime::argv`]). Nothing between the dashboard/MCP tool and the
//! container checks those flags, so one the image does not have — or an enum
//! value that drifted underneath us (`--spec-type draft` for what is now
//! `draft-dflash`) — surfaces much later, at model load, as an opaque
//! llama-server error attached to no particular field.
//!
//! `llama-server --help` is the only authority that always matches the image
//! actually installed — and since per-model image overrides (§3.1) there is
//! one vocabulary *per image*, which is why the cache that feeds this parser
//! is keyed by image ID ([`crate::runtime::registry::Registry::help_text`]).
//! This module turns that text into a set that answers two
//! questions: *does this build take this flag* and *is this a legal value for
//! it*. That buys us
//!   * validation at configuration time ([`LlamaCaps::validate_args`],
//!     [`LlamaCaps::validate_pair`]), and
//!   * a published vocabulary, so tool schemas can carry the image's own enums
//!     instead of hardcoded lists that rot with the next llama.cpp bump.
//!
//! The module is deliberately **pure**: it parses text and spawns nothing.
//! Running the binary is the caller's job (the crate's `CommandRunner`), which
//! keeps process handling in one place and keeps this file testable against
//! pasted help output.
//!
//! # The shape of the help text
//!
//! Entries start in column 0 with `-`; descriptions start at a fixed column
//! (40 in every build seen so far, derived from the text rather than assumed)
//! and wrap into indented continuation lines:
//!
//! ```text
//! -fit,  --fit [on|off]                   whether to adjust unset arguments to fit in device memory ('on' or
//!                                         'off', default: 'on')
//! --spec-draft-model, -md, --model-draft FNAME
//!                                         draft model for speculative decoding (default: unused)
//! ```
//!
//! An entry declares any number of comma-separated aliases (short and long,
//! mixed) optionally followed by a value placeholder. When the alias list is
//! wide enough to reach the description column, the description begins on the
//! next line instead — hence the column arithmetic in [`split_head`].
//!
//! # Where enum values come from, and how much they are trusted
//!
//! Five sources, in descending precedence (see [`EnumKind`]):
//!
//! | source | example |
//! |---|---|
//! | value list glued to the flag | `--spec-type none,draft-simple,draft-mtp,…` |
//! | bracketed placeholder | `--fit [on|off]`, `--rope-scaling {none,linear,yarn}` |
//! | `allowed values:` line | `--cache-type-k TYPE` → `f32, f16, bf16, …` |
//! | `one of:` + `- name: description` bullets | `--reasoning-format FORMAT` |
//! | single-quoted tokens in prose | `--spec-draft-ngl` → `'auto'`, `'all'` |
//!
//! The last one is a **guess** and is never exhaustive — `--spec-draft-ngl`
//! also takes a plain layer count — so guessed lists are recorded in
//! [`LlamaCaps::soft_enums`] and are used as hints only: validation never
//! rejects a value against them, and a schema generator must emit them as a
//! description, not as a JSON-Schema `enum`.
//!
//! # Deprecated is not removed
//!
//! `--draft-max` prints "the argument has been removed" and *fails*;
//! `--mlock` prints "DEPRECATED in favor of `--load-mode`" and still works.
//! Only the first kind lands in [`LlamaCaps::removed`] and is reported as a
//! problem; deprecations are exposed separately so a caller can warn without
//! blocking a config that llama-server would happily load.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

// ---------------------------------------------------------------------------
// Managed flags
// ---------------------------------------------------------------------------

/// llama-server flags that lmgw renders from dedicated struct fields.
/// Passing one of these through the freeform extra-args escape hatch would
/// silently produce a duplicate preset key, so callers reject them there.
pub const MANAGED_FLAGS: &[&str] = &[
    "model",
    "mmproj",
    "ctx-size",
    "n-predict",
    "predict",
    "n",
    "n-gpu-layers",
    "threads",
    "batch-size",
    "ubatch-size",
    "parallel",
    "kv-unified",
    "no-kv-unified",
    "kv-unified-per-slot",
    "flash-attn",
    "cache-type-k",
    "cache-type-v",
    "cache-ram",
    "jinja",
    "temp",
    "temperature",
    "top-p",
    "top-k",
    "min-p",
    "repeat-penalty",
    "presence-penalty",
    "seed",
    "model-draft",
    "spec-draft-model",
    "spec-type",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-ngl",
    "sleep-idle-seconds",
    "alias",
    "reasoning-format",
    "reasoning",
    "reasoning-budget",
    "reasoning-preserve",
    "no-reasoning-preserve",
    "reasoning-effort",
    "chat-template-file",
    "chat-template-kwargs",
    "fit",
    "fit-ctx",
];

/// True when `flag` is rendered from a dedicated field ([`MANAGED_FLAGS`]).
/// Accepts the flag with or without leading dashes.
pub fn is_managed(flag: &str) -> bool {
    let name = strip_dashes(flag);
    MANAGED_FLAGS.contains(&name)
}

// ---------------------------------------------------------------------------
// Capability set
// ---------------------------------------------------------------------------

/// What one `llama-server` build accepts, as parsed from its `--help`.
///
/// Every field is keyed by a flag name **without leading dashes**, and every
/// alias of an entry is registered separately, so `model-draft` and
/// `spec-draft-model` both resolve to the same enum/arity/removal facts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LlamaCaps {
    /// Every long flag seen, without leading dashes: "ctx-size", "spec-type",
    /// "mmproj", … Includes all aliases of a flag as separate entries.
    pub flags: BTreeSet<String>,
    /// Single-dash forms (`-ngl`, `-md`, `-fa`), without the dash. Kept apart
    /// from [`Self::flags`] because a preset key is always a long flag, while
    /// freeform extra-args may legitimately use the short form.
    pub short_flags: BTreeSet<String>,
    /// Flags documented with a fixed value list, e.g. spec-type -> [none,
    /// draft-simple, …]. Values are in the order the help text lists them.
    pub enums: BTreeMap<String, Vec<String>>,
    /// Flags whose entry in [`Self::enums`] was inferred from prose and is
    /// therefore *not* exhaustive — hints only, never grounds for rejection.
    pub soft_enums: BTreeSet<String>,
    /// Flags whose value is a comma-separated *subset* of their enum
    /// (`--spec-type draft-dflash,ngram-mod`), not a single member.
    pub list_enums: BTreeSet<String>,
    /// Flags that take a value: the entry carried a placeholder or an enum.
    /// Everything else is treated as a boolean switch.
    pub value_flags: BTreeSet<String>,
    /// Flags explicitly marked as removed in the help text ("the argument has
    /// been removed"). These fail at startup; see [`Self::deprecated`] for the
    /// softer kind.
    pub removed: BTreeSet<String>,
    /// Flags the help marks DEPRECATED. They still work, so they are reported
    /// nowhere by the validators — surface them as a warning if you want one.
    pub deprecated: BTreeSet<String>,
    /// Help text of removed/deprecated flags, carrying the replacement advice.
    pub notes: BTreeMap<String, String>,
}

impl LlamaCaps {
    /// True when this build knows `flag`, given with or without leading
    /// dashes, in either its long or its short form.
    pub fn supports(&self, flag: &str) -> bool {
        let name = strip_dashes(flag);
        self.flags.contains(name) || self.short_flags.contains(name)
    }

    /// True when the help says the flag was *removed* (a hard failure), as
    /// opposed to merely deprecated.
    pub fn is_removed(&self, flag: &str) -> bool {
        self.removed.contains(strip_dashes(flag))
    }

    /// True when the help marks the flag DEPRECATED. Such flags still work.
    pub fn is_deprecated(&self, flag: &str) -> bool {
        self.deprecated.contains(strip_dashes(flag))
    }

    /// The documented values of `flag`, if it has any. Check
    /// [`Self::soft_enums`] before treating the result as exhaustive.
    pub fn values_for(&self, flag: &str) -> Option<&[String]> {
        self.enums.get(strip_dashes(flag)).map(Vec::as_slice)
    }

    /// The help text kept for a removed or deprecated flag, if any.
    pub fn note_for(&self, flag: &str) -> Option<&str> {
        self.notes.get(strip_dashes(flag)).map(String::as_str)
    }

    /// Validate a parsed extra-args token list (`["--mmproj", "/models/x.gguf",
    /// "--no-warmup"]`). Returns one human-readable problem per offending
    /// token. Empty vec = fine.
    ///
    /// Token→value attachment mirrors the argv renderer's exactly: an
    /// option claims the next token unless that token is itself an option
    /// (negative numbers are values, not options). Diverging here would mean
    /// validating a different command line than the one we render.
    pub fn validate_args(&self, args: &[String]) -> Vec<String> {
        let mut problems = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let tok = args[i].trim();
            i += 1;
            if tok.is_empty() {
                continue;
            }
            if !is_opt(tok) {
                // A bare `KEY=value` token is a legal preset line on its own
                // (the env-var form), so it is never a stray value.
                if !tok.contains('=') {
                    problems.push(format!("stray value `{tok}` (not attached to a flag)"));
                }
                continue;
            }
            let (name, inline) = match tok.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (tok, None),
            };
            let key = strip_dashes(name);
            if key.is_empty() {
                problems.push(format!("stray token `{tok}` (not a flag)"));
                continue;
            }
            if !self.supports(key) {
                problems.push(self.unknown_message(name));
                // Swallow the value the unknown flag probably owns, so one
                // typo yields one problem instead of two.
                if inline.is_none() && i < args.len() && !is_opt(&args[i]) {
                    i += 1;
                }
                continue;
            }
            if let Some(msg) = self.removed_message(name, key) {
                problems.push(msg);
            }
            let takes_value = self.value_flags.contains(key);
            let value = match inline {
                Some(v) => Some(v),
                None if takes_value && i < args.len() && !is_opt(&args[i]) => {
                    let v = args[i].as_str();
                    i += 1;
                    Some(v)
                }
                None => None,
            };
            match value {
                Some(v) => {
                    if let Some(msg) = self.enum_message(name, key, v) {
                        problems.push(msg);
                    }
                }
                None if takes_value => problems.push(format!("`{name}` expects a value")),
                None => {}
            }
        }
        problems
    }

    /// Validate a single key=value preset pair. Used for the structured fields.
    ///
    /// A value on a boolean flag is *not* a problem: presets spell switches as
    /// `jinja = true`, which is how llama-server wants them.
    pub fn validate_pair(&self, key: &str, value: Option<&str>) -> Option<String> {
        let name = if key.starts_with('-') {
            key.to_string()
        } else {
            format!("--{key}")
        };
        let flag = strip_dashes(key);
        if flag.is_empty() {
            return Some("empty preset key".to_string());
        }
        if !self.supports(flag) {
            return Some(self.unknown_message(&name));
        }
        if let Some(msg) = self.removed_message(&name, flag) {
            return Some(msg);
        }
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => self.enum_message(&name, flag, v),
            None if self.value_flags.contains(flag) => Some(format!("`{name}` expects a value")),
            None => None,
        }
    }

    /// `unknown llama-server flag \`--mmprj\` (did you mean \`--mmproj\`?)`
    fn unknown_message(&self, name: &str) -> String {
        let base = format!("unknown llama-server flag `{name}`");
        match self.suggest(name) {
            Some(hit) => format!("{base} (did you mean `{hit}`?)"),
            None => base,
        }
    }

    fn removed_message(&self, name: &str, flag: &str) -> Option<String> {
        if !self.removed.contains(flag) {
            return None;
        }
        let hint = self
            .notes
            .get(flag)
            .map(|n| replacement_hint(n))
            .filter(|h| !h.is_empty());
        Some(match hint {
            Some(h) => format!("`{name}` was removed from this llama-server build ({h})"),
            None => format!("`{name}` was removed from this llama-server build"),
        })
    }

    fn enum_message(&self, name: &str, flag: &str, value: &str) -> Option<String> {
        let values = self.enums.get(flag)?;
        // Guessed lists are incomplete by construction; rejecting against them
        // would flag perfectly valid configs (`--spec-draft-ngl 40`).
        if self.soft_enums.contains(flag) {
            return None;
        }
        let ok = if self.list_enums.contains(flag) {
            value
                .split(',')
                .all(|p| values.iter().any(|v| v == p.trim()))
        } else {
            values.iter().any(|v| v == value)
        };
        if ok {
            None
        } else {
            Some(format!(
                "`{name} {value}` is not one of: {}",
                values.join(", ")
            ))
        }
    }

    /// Closest known flag to a mistyped one, rendered with its dashes.
    /// Long forms win ties: a preset key is always a long flag.
    fn suggest(&self, name: &str) -> Option<String> {
        let needle = strip_dashes(name);
        if needle.is_empty() {
            return None;
        }
        let mut best: Option<((usize, usize, usize), String)> = None;
        let candidates = self
            .flags
            .iter()
            .map(|f| (f, true))
            .chain(self.short_flags.iter().map(|f| (f, false)));
        for (cand, is_long) in candidates {
            let mut dist = levenshtein(needle, cand);
            // A prefix/substring hit is a strong signal even when the edit
            // distance is large (`--ctx` → `--ctx-size`). Both sides must be
            // long enough for the containment to mean anything: every flag
            // contains `-m`.
            let containment = cand.contains(needle) || needle.contains(cand.as_str());
            if needle.len() >= 3 && cand.len() >= 3 && containment {
                dist = dist.min(1);
            }
            let budget = if needle.len() <= 6 { 2 } else { 3 };
            if dist > budget {
                continue;
            }
            // Deterministic pick: closest first, then long forms over short
            // ones (a preset key is always a long flag), then the shortest.
            let score = (dist, usize::from(!is_long), cand.len());
            if best.as_ref().is_none_or(|(b, _)| score < *b) {
                let dashes = if is_long { "--" } else { "-" };
                best = Some((score, format!("{dashes}{cand}")));
            }
        }
        best.map(|(_, hit)| hit)
    }
}

// ---------------------------------------------------------------------------
// Help parsing
// ---------------------------------------------------------------------------

/// Column the description starts in when a help build differs from the usual
/// layout and the text gives us nothing to measure.
const DEFAULT_DESC_COLUMN: usize = 40;
/// Indents below this are prose, not the description column.
const MIN_DESC_COLUMN: usize = 8;

/// Parse `llama-server --help` output into a capability set.
pub fn parse_help(text: &str) -> LlamaCaps {
    let column = desc_column(text);
    let mut caps = LlamaCaps::default();
    for entry in parse_entries(text, column) {
        absorb(&mut caps, entry);
    }
    caps
}

/// One help entry: its aliases, its value placeholder (or glued value list),
/// and its description lines with the `(env: …)` noise already dropped.
#[derive(Debug, Default)]
struct Entry {
    long: Vec<String>,
    short: Vec<String>,
    rest: String,
    desc: Vec<String>,
}

/// Where an enum came from — which decides how far we trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumKind {
    /// Values glued to the flag: `--spec-type none,draft-simple,…`.
    ValueList,
    /// Bracketed placeholder: `[on|off]`, `{none,linear,yarn}`, `<0|1>`.
    Placeholder,
    /// An `allowed values: …` continuation line.
    AllowedValues,
    /// `- name: description` bullets.
    Bullets,
    /// Single-quoted tokens mined from prose. Never exhaustive.
    Guess,
}

impl EnumKind {
    /// Guessed lists must not be used to reject a value.
    fn is_soft(self) -> bool {
        matches!(self, EnumKind::Guess)
    }

    /// Description-derived lists may be missing the default, which the help
    /// often only names later in `(default: X)`.
    fn may_omit_default(self) -> bool {
        matches!(
            self,
            EnumKind::AllowedValues | EnumKind::Bullets | EnumKind::Guess
        )
    }
}

/// Fold one parsed entry into the capability set, registering every alias.
fn absorb(caps: &mut LlamaCaps, entry: Entry) {
    let joined = entry.desc.join(" ");
    let takes_value = !entry.rest.trim().is_empty();

    let mut values = enum_from_rest(&entry.rest).or_else(|| enum_from_desc(&entry.desc, &joined));
    if let Some((vals, kind)) = values.as_mut() {
        if kind.may_omit_default() {
            // `--reasoning-format` lists none/deepseek/deepseek-legacy as
            // bullets and only mentions `auto` in the trailing default.
            if let Some(d) = default_marker(&joined) {
                if !vals.contains(&d) {
                    vals.push(d);
                }
            }
        }
    }

    let removed = joined.contains("has been removed");
    let deprecated = joined.contains("DEPRECATED");

    let names = entry
        .long
        .iter()
        .map(|n| (n, true))
        .chain(entry.short.iter().map(|n| (n, false)));
    for (name, is_long) in names {
        if is_long {
            caps.flags.insert(name.clone());
        } else {
            caps.short_flags.insert(name.clone());
        }
        if takes_value {
            caps.value_flags.insert(name.clone());
        }
        if let Some((vals, kind)) = &values {
            caps.enums.insert(name.clone(), vals.clone());
            if kind.is_soft() {
                caps.soft_enums.insert(name.clone());
            }
            if *kind == EnumKind::ValueList {
                caps.list_enums.insert(name.clone());
            }
        }
        if removed {
            caps.removed.insert(name.clone());
        }
        if deprecated {
            caps.deprecated.insert(name.clone());
        }
        if (removed || deprecated) && !joined.is_empty() {
            caps.notes.insert(name.clone(), joined.clone());
        }
    }
}

/// The column descriptions start in, measured from the continuation lines
/// (the most common indent). Measuring beats hardcoding: the width is a
/// build-time constant in llama.cpp and has moved before.
fn desc_column(text: &str) -> usize {
    let mut histogram: BTreeMap<usize, usize> = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        if indent >= MIN_DESC_COLUMN {
            *histogram.entry(indent).or_default() += 1;
        }
    }
    histogram
        .into_iter()
        // Most frequent indent; on a tie the narrower one (bullet lines sit
        // one column further right than the description proper).
        .max_by_key(|&(indent, count)| (count, usize::MAX - indent))
        .map(|(indent, _)| indent)
        .unwrap_or(DEFAULT_DESC_COLUMN)
}

/// Split the help into entries. A new entry starts in column 0 with `-`;
/// everything indented belongs to the entry above it.
fn parse_entries(text: &str, column: usize) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let starts_entry = line.starts_with('-');
        if starts_entry {
            let (spec, inline_desc) = split_head(line, column);
            open = false;
            // `----- common params -----` looks like an entry but parses to no
            // usable alias, which is exactly how we drop it.
            if let Some(mut entry) = parse_spec(spec) {
                if !inline_desc.trim().is_empty() {
                    entry.desc.push(inline_desc.trim().to_string());
                }
                entries.push(entry);
                open = true;
            }
            continue;
        }
        if !open {
            continue;
        }
        // Continuations are indented. A line starting at column 0 that is not
        // a flag ends the entry instead of joining it — otherwise a trailing
        // usage epilogue is read as the last flag's description, which can
        // forge a `removed` mark or an enum for it.
        if !line.starts_with(char::is_whitespace) {
            open = false;
            continue;
        }
        let cont = line.trim();
        // `(env: LLAMA_ARG_*)` lines are commas and capitals that would
        // pollute every value-list scan below.
        if cont.starts_with("(env:") {
            continue;
        }
        if let Some(entry) = entries.last_mut() {
            entry.desc.push(cont.to_string());
        }
    }
    entries
}

/// Cut an entry's head line into (alias spec, inline description).
///
/// The description starts exactly at `column`, padded with spaces. When the
/// alias list reaches that far the padding is gone and llama.cpp wraps the
/// description onto the next line instead — detected by the character before
/// the column not being padding.
fn split_head(line: &str, column: usize) -> (&str, &str) {
    match line.char_indices().nth(column) {
        None => (line, ""),
        Some((byte, _)) => {
            if line[..byte].ends_with(' ') {
                (&line[..byte], &line[byte..])
            } else {
                (line, "")
            }
        }
    }
}

/// Parse `-md, --model-draft FNAME` into aliases plus whatever trails them.
///
/// Aliases are consumed left to right for as long as the comma-separated
/// elements start with `-`; the first element that does not ends the alias
/// list and begins the placeholder / value-list region. That is the whole
/// difference between `--spec-draft-model, -md, --model-draft FNAME` (three
/// aliases) and `--spec-type none,draft-simple,…` (one alias, ten values).
fn parse_spec(spec: &str) -> Option<Entry> {
    let mut entry = Entry::default();
    let mut cur = spec.trim();
    loop {
        cur = cur.trim_start();
        if !cur.starts_with('-') {
            break;
        }
        let end = cur
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(cur.len());
        let token = &cur[..end];
        let name = strip_dashes(token);
        if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            return None; // section rule, not a flag
        }
        if token.starts_with("--") {
            entry.long.push(name.to_string());
        } else {
            entry.short.push(name.to_string());
        }
        let after = &cur[end..];
        match after.strip_prefix(',') {
            Some(next) => cur = next,
            None => {
                cur = after.trim();
                break;
            }
        }
    }
    if entry.long.is_empty() && entry.short.is_empty() {
        return None;
    }
    entry.rest = cur.trim().to_string();
    Some(entry)
}

/// Enum values taken from what follows the aliases.
fn enum_from_rest(rest: &str) -> Option<(Vec<String>, EnumKind)> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    // Value list glued to the flag. Placeholders that also contain commas
    // (`N0,N1,N2,...`, `<dev1,dev2,..>`, `KEY=TYPE:VALUE,...`) are excluded by
    // requiring every element to look like a literal value — placeholders are
    // uppercase or bracketed, values are lowercase identifiers.
    if !rest.contains(char::is_whitespace) && rest.contains(',') {
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() >= 2 && parts.iter().all(|p| is_value_token(p)) {
            let vals = parts.into_iter().map(str::to_string).collect();
            return Some((vals, EnumKind::ValueList));
        }
    }
    // Bracketed placeholders. `[on|off]` and `{none,linear,yarn}` are enums;
    // `<0...100>`, `<dev1,dev2,..>` and `[<repo>/]<model>[:quant]` are not,
    // and fall out because their elements are not value tokens.
    // Bracket style carries the confidence. `[on|off]` enumerates the legal
    // values; angle brackets are a *type hint*, and llama.cpp writes `<0|1>`
    // for flags whose value is an ordinary int — `--poll-batch <0|1>` defaults
    // to 50. Rejecting against an angle-bracket list therefore blocks values
    // the build accepts, so those are advisory only.
    let (inner, sep, kind) = match (rest.chars().next(), rest.chars().last()) {
        (Some('['), Some(']')) => (&rest[1..rest.len() - 1], '|', EnumKind::Placeholder),
        (Some('{'), Some('}')) => (&rest[1..rest.len() - 1], ',', EnumKind::Placeholder),
        (Some('<'), Some('>')) => (&rest[1..rest.len() - 1], '|', EnumKind::Guess),
        _ => return None,
    };
    let parts: Vec<&str> = inner.split(sep).collect();
    if parts.len() >= 2 && parts.iter().all(|p| is_value_token(p)) {
        let vals = parts.into_iter().map(str::to_string).collect();
        return Some((vals, kind));
    }
    None
}

/// Enum values mined from the description, in descending confidence.
fn enum_from_desc(desc: &[String], joined: &str) -> Option<(Vec<String>, EnumKind)> {
    const MARKER: &str = "allowed values:";
    if let Some(idx) = joined.find(MARKER) {
        let tail = &joined[idx + MARKER.len()..];
        // The list runs to the first parenthesis — `(default: f16)` follows it.
        let tail = tail.split('(').next().unwrap_or(tail);
        let parts: Vec<&str> = tail.split(',').map(str::trim).collect();
        if parts.len() >= 2 && parts.iter().all(|p| is_value_token(p)) {
            let vals = parts.into_iter().map(str::to_string).collect();
            return Some((vals, EnumKind::AllowedValues));
        }
    }
    let bullets = bullet_names(desc);
    if bullets.len() >= 2 {
        // Numeric bullets (`--verbosity`: `- 0: generic output`) document a
        // range as much as a set, so they are hints rather than a closed enum.
        let numeric = bullets
            .iter()
            .all(|b| b.chars().all(|c| c.is_ascii_digit()));
        let kind = if numeric {
            EnumKind::Guess
        } else {
            EnumKind::Bullets
        };
        return Some((bullets, kind));
    }
    let quoted = quoted_tokens(joined);
    if quoted.len() >= 2 {
        return Some((quoted, EnumKind::Guess));
    }
    None
}

/// Names of `- name: description` bullets, with a trailing `(default)` marker
/// dropped (`- layer (default): split layers …`).
fn bullet_names(desc: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for line in desc {
        let Some(rest) = line.trim().strip_prefix("- ") else {
            continue;
        };
        let Some((head, _)) = rest.split_once(':') else {
            continue;
        };
        let name = head.split_whitespace().next().unwrap_or_default();
        if is_value_token(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Single-quoted identifier-shaped tokens: `either an exact number, 'auto', or
/// 'all'`. Odd segments of a split on `'` are the quoted ones; anything with a
/// space or punctuation in it (`'\n'`, `';'`) is rejected by [`is_value_token`],
/// which is also what keeps stray apostrophes ("they're") from matching.
fn quoted_tokens(joined: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, seg) in joined.split('\'').enumerate() {
        if i % 2 == 1 && is_value_token(seg) && !out.iter().any(|v| v == seg) {
            out.push(seg.to_string());
        }
    }
    out
}

/// The value named by the first `(default: X)` marker, unquoted.
fn default_marker(joined: &str) -> Option<String> {
    const MARKER: &str = "(default:";
    let idx = joined.find(MARKER)?;
    let tail = &joined[idx + MARKER.len()..];
    let token = tail.split_whitespace().next()?;
    let token = token.trim_matches(|c: char| "'\")(,;.".contains(c));
    is_value_token(token).then(|| token.to_string())
}

/// The replacement advice inside a removal note: everything the help says
/// after "…has been removed."
fn replacement_hint(note: &str) -> String {
    match note.split_once("has been removed") {
        Some((_, rest)) => rest.trim_start_matches(['.', ' ']).trim().to_string(),
        None => note.trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// A flag name without its leading dashes.
fn strip_dashes(flag: &str) -> &str {
    flag.trim_start_matches('-')
}

/// True for tokens that start an option (`--ctx-size`, `-ngl`) as opposed to
/// values; negative numbers (`-1`) are values. Mirrors
/// [`crate::runtime::argv::is_opt`] so validation and argv rendering agree on
/// what owns what.
fn is_opt(token: &str) -> bool {
    token.starts_with('-') && token.parse::<f64>().is_err()
}

/// True for tokens that look like a literal enum value: a lowercase
/// identifier (`draft-dflash`, `q8_0`, `mmap+mlock`, `0`). Uppercase and
/// bracketed shapes are placeholders (`FNAME`, `N`, `<dev1,dev2,..>`), and
/// `...` continuations are neither.
fn is_value_token(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    token
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Plain Levenshtein distance over two rolling rows — the flag list is a few
/// hundred short strings, so nothing cleverer is warranted.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim excerpt of `podman run --rm --entrypoint /app/llama-server
    /// <image> --help`, picked to hit every parsing branch:
    /// section rule, boolean alias lists, placeholders, glued value lists,
    /// `allowed values:`, bullets, prose quotes, removals and deprecations.
    /// Column alignment is load-bearing — do not reflow.
    const SAMPLE_HELP: &str = r#"
----- common params -----

-h,    --help, --usage                  print usage and exit
-t,    --threads N                      number of CPU threads to use during generation (default: -1)
                                        (env: LLAMA_ARG_THREADS)
-c,    --ctx-size N                     size of the prompt context (default: 0, 0 = loaded from model)
                                        (env: LLAMA_ARG_CTX_SIZE)
-fa,   --flash-attn [on|off|auto]       set Flash Attention use ('on', 'off', or 'auto', default: 'auto')
                                        (env: LLAMA_ARG_FLASH_ATTN)
-e,    --escape, --no-escape            whether to process escapes sequences (\n, \r, \t, \', \", \\)
                                        (default: true)
--rope-scaling {none,linear,yarn}       RoPE frequency scaling method, defaults to linear unless specified by
                                        the model
                                        (env: LLAMA_ARG_ROPE_SCALING_TYPE)
-ctk,  --cache-type-k TYPE              KV cache data type for K
                                        allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1
                                        (default: f16)
                                        (env: LLAMA_ARG_CACHE_TYPE_K)
-dt,   --defrag-thold N                 KV cache defragmentation threshold (DEPRECATED)
                                        (env: LLAMA_ARG_DEFRAG_THOLD)
-lm,   --load-mode MODE                 model loading mode (default: mmap)
                                        - none: no special loading mode
                                        - mmap: memory-map model (if mmap disabled, slower load but may reduce
                                        pageouts if not using mlock)
                                        - mlock: force system to keep model in RAM rather than swapping or
                                        compressing
                                        - mmap+mlock: mmap + force system to keep model in RAM rather than
                                        swapping or compressing
                                        - dio: use DirectIO if available
                                        
                                        (env: LLAMA_ARG_LOAD_MODE)
-dev,  --device <dev1,dev2,..>          comma-separated list of devices to use for offloading (none = don't
                                        offload)
                                        use --list-devices to see a list of available devices
                                        (env: LLAMA_ARG_DEVICE)
-ot,   --override-tensor <tensor name pattern>=<buffer type>,...
                                        override tensor buffer type
                                        (env: LLAMA_ARG_OVERRIDE_TENSOR)
-ngl,  --gpu-layers, --n-gpu-layers N   max. number of layers to store in VRAM, either an exact number,
                                        'auto', or 'all' (default: auto)
                                        (env: LLAMA_ARG_N_GPU_LAYERS)
-fit,  --fit [on|off]                   whether to adjust unset arguments to fit in device memory ('on' or
                                        'off', default: 'on')
                                        (env: LLAMA_ARG_FIT)
-m,    --model FNAME                    model path to load
                                        (env: LLAMA_ARG_MODEL)
--spec-draft-type-k, -ctkd, --cache-type-k-draft TYPE
                                        KV cache data type for K for the draft model
                                        allowed values: f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1
                                        (default: f16)
                                        (env: LLAMA_ARG_SPEC_DRAFT_CACHE_TYPE_K)
--spec-draft-n-max N                    number of tokens to draft for speculative decoding (default: 3)
                                        (env: LLAMA_ARG_SPEC_DRAFT_N_MAX)
--spec-draft-n-min N                    minimum number of draft tokens to use for speculative decoding
                                        (default: 0)
                                        (env: LLAMA_ARG_SPEC_DRAFT_N_MIN)
--spec-draft-ngl, -ngld, --gpu-layers-draft, --n-gpu-layers-draft N
                                        max. number of draft model layers to store in VRAM, either an exact
                                        number, 'auto', or 'all' (default: auto)
                                        (env: LLAMA_ARG_N_GPU_LAYERS_DRAFT)
--spec-draft-model, -md, --model-draft FNAME
                                        draft model for speculative decoding (default: unused)
                                        (env: LLAMA_ARG_SPEC_DRAFT_MODEL)
--spec-type none,draft-simple,draft-eagle3,draft-mtp,draft-dflash,draft-dspark,ngram-simple,ngram-map-k,ngram-map-k4v,ngram-mod,ngram-cache
                                        comma-separated list of types of speculative decoding to use (default:
                                        none)
                                        
                                        (env: LLAMA_ARG_SPEC_TYPE)
--draft, --draft-n, --draft-max N       the argument has been removed. use --spec-draft-n-max or
                                        --spec-ngram-mod-n-max
                                        (env: LLAMA_ARG_DRAFT_MAX)
--draft-min, --draft-n-min N            the argument has been removed. use --spec-draft-n-min or
                                        --spec-ngram-mod-n-min
                                        (env: LLAMA_ARG_DRAFT_MIN)
--warmup, --no-warmup                   whether to perform warmup with an empty run (default: enabled)
-mm,   --mmproj FILE                    path to a multimodal projector file. see tools/mtmd/README.md
                                        note: if -hf is used, this argument can be omitted
                                        (env: LLAMA_ARG_MMPROJ)
--mmproj-auto, --no-mmproj, --no-mmproj-auto
                                        whether to use multimodal projector file (if available), useful when
                                        using -hf (default: enabled)
                                        (env: LLAMA_ARG_MMPROJ_AUTO)
--jinja, --no-jinja                     whether to use jinja template engine for chat (default: enabled)
                                        (env: LLAMA_ARG_JINJA)
--reasoning-format FORMAT               controls whether thought tags are allowed and/or extracted from the
                                        response, and in which format they're returned; one of:
                                        - none: leaves thoughts unparsed in `message.content`
                                        - deepseek: puts thoughts in `message.reasoning_content`
                                        - deepseek-legacy: keeps `<think>` tags in `message.content` while
                                        also populating `message.reasoning_content`
                                        (default: auto)
                                        (env: LLAMA_ARG_THINK)
-rea,  --reasoning [on|off|auto]        Use reasoning/thinking in the chat ('on', 'off', or 'auto', default:
                                        'auto' (detect from template))
                                        (env: LLAMA_ARG_REASONING)
--sleep-idle-seconds SECONDS            number of seconds of idleness after which the server will sleep
                                        (default: -1; -1 = disabled)
"#;

    fn caps() -> LlamaCaps {
        parse_help(SAMPLE_HELP)
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn section_rules_are_not_flags() {
        let caps = caps();
        assert!(!caps
            .flags
            .iter()
            .any(|f| f.is_empty() || f.starts_with('-')));
        assert!(!caps.supports("common"));
    }

    #[test]
    fn supports_long_short_and_dashed_forms() {
        let caps = caps();
        assert!(caps.supports("spec-type"));
        assert!(caps.supports("--mmproj"));
        assert!(caps.supports("mmproj"));
        assert!(caps.supports("-mm"), "short forms are known too");
        assert!(!caps.supports("--not-a-flag"));
    }

    #[test]
    fn every_alias_of_an_entry_is_registered() {
        let caps = caps();
        assert!(caps.supports("model-draft"));
        assert!(caps.supports("spec-draft-model"));
        assert!(caps.supports("gpu-layers-draft"));
        assert!(caps.supports("n-gpu-layers-draft"));
        // Entries whose alias list reaches the description column carry their
        // description on the next line; they must still parse.
        assert!(caps.supports("mmproj-auto"));
        assert!(caps.supports("no-mmproj"));
    }

    #[test]
    fn glued_value_list_is_values_not_aliases() {
        let caps = caps();
        let vals = caps.values_for("spec-type").expect("spec-type has an enum");
        assert!(vals.iter().any(|v| v == "draft-dflash"));
        assert!(vals.iter().any(|v| v == "draft-mtp"));
        assert!(vals.iter().any(|v| v == "ngram-cache"));
        assert!(!vals.iter().any(|v| v == "-md" || v == "md"));
        // …and the values must not have been mistaken for flags.
        assert!(!caps.supports("draft-simple"));
        assert!(!caps.supports("ngram-cache"));
    }

    #[test]
    fn alias_list_is_aliases_not_values() {
        let caps = caps();
        // The mirror image of the previous test: every element starts with
        // `-`, so nothing here may end up as an enum value.
        assert!(caps.values_for("model-draft").is_none());
        assert!(caps.values_for("jinja").is_none());
        assert!(caps.supports("no-jinja"));
    }

    #[test]
    fn bracket_placeholders_are_enums() {
        let caps = caps();
        assert_eq!(caps.values_for("fit").unwrap(), ["on", "off"]);
        assert_eq!(
            caps.values_for("reasoning").unwrap(),
            ["on", "off", "auto"],
            "bracket form mixed with a short alias"
        );
        assert_eq!(
            caps.values_for("flash-attn").unwrap(),
            ["on", "off", "auto"]
        );
        assert_eq!(
            caps.values_for("rope-scaling").unwrap(),
            ["none", "linear", "yarn"],
            "brace form"
        );
    }

    #[test]
    fn allowed_values_line_is_an_enum() {
        let caps = caps();
        let expect = [
            "f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
        ];
        assert_eq!(caps.values_for("cache-type-k").unwrap(), expect);
        // Same entry reached through its other aliases.
        assert_eq!(caps.values_for("cache-type-k-draft").unwrap(), expect);
        assert_eq!(caps.values_for("spec-draft-type-k").unwrap(), expect);
        assert_eq!(caps.values_for("-ctkd").unwrap(), expect);
    }

    #[test]
    fn one_of_bullets_are_an_enum_including_the_default() {
        let caps = caps();
        let vals = caps.values_for("reasoning-format").unwrap();
        assert!(vals.iter().any(|v| v == "none"));
        assert!(vals.iter().any(|v| v == "deepseek"));
        assert!(vals.iter().any(|v| v == "deepseek-legacy"));
        assert!(
            vals.iter().any(|v| v == "auto"),
            "`auto` only appears in the trailing (default: auto): {vals:?}"
        );
        assert!(!caps.soft_enums.contains("reasoning-format"));
    }

    #[test]
    fn bullets_without_a_one_of_cue_still_parse() {
        let caps = caps();
        let vals = caps.values_for("load-mode").unwrap();
        assert!(vals.iter().any(|v| v == "mmap+mlock"), "{vals:?}");
        assert!(vals.iter().any(|v| v == "dio"));
    }

    #[test]
    fn prose_quotes_are_a_soft_enum() {
        let caps = caps();
        let vals = caps.values_for("spec-draft-ngl").unwrap();
        assert!(vals.iter().any(|v| v == "auto"), "{vals:?}");
        assert!(vals.iter().any(|v| v == "all"), "{vals:?}");
        assert!(
            caps.soft_enums.contains("spec-draft-ngl"),
            "prose lists are never exhaustive"
        );
    }

    #[test]
    fn prose_scanner_ignores_stray_quotes_and_escapes() {
        let caps = caps();
        // `--escape` describes `(\n, \r, \t, \', \", \\)` — no enum there.
        assert!(caps.values_for("escape").is_none());
        assert!(caps.values_for("model").is_none());
        assert!(caps.values_for("override-tensor").is_none());
        assert!(caps.values_for("device").is_none(), "<dev1,dev2,..>");
    }

    #[test]
    fn removed_and_deprecated_are_distinguished() {
        let caps = caps();
        assert!(caps.is_removed("draft-max"));
        assert!(caps.is_removed("draft"));
        assert!(caps.is_removed("draft-n"));
        assert!(caps.is_removed("--draft-min"));
        assert!(!caps.is_removed("spec-draft-n-max"));
        assert!(caps.supports("spec-draft-n-max"));
        // DEPRECATED still works, so it must not be reported as removed.
        assert!(caps.is_deprecated("defrag-thold"));
        assert!(!caps.is_removed("defrag-thold"));
        assert!(caps
            .validate_args(&v(&["--defrag-thold", "0.1"]))
            .is_empty());
    }

    #[test]
    fn value_arity_comes_from_placeholders() {
        let caps = caps();
        assert!(caps.value_flags.contains("ctx-size"));
        assert!(caps.value_flags.contains("mmproj"));
        assert!(caps.value_flags.contains("spec-type"));
        assert!(!caps.value_flags.contains("jinja"));
        assert!(!caps.value_flags.contains("no-mmproj"));
        assert!(!caps.value_flags.contains("help"));
    }

    #[test]
    fn validate_args_accepts_a_normal_line() {
        let caps = caps();
        assert!(caps
            .validate_args(&v(&["--jinja", "--ctx-size", "4096"]))
            .is_empty());
        assert!(caps.validate_args(&v(&["--no-mmproj"])).is_empty());
        assert!(caps
            .validate_args(&v(&["-ngl", "999", "-fa", "on"]))
            .is_empty());
        // Negative numbers are values, not options.
        assert!(caps
            .validate_args(&v(&["--sleep-idle-seconds", "-1"]))
            .is_empty());
        // `--key=value` is a legal preset token, as is the bare env form.
        assert!(caps.validate_args(&v(&["--ctx-size=4096"])).is_empty());
        assert!(caps
            .validate_args(&v(&["LLAMA_ARG_CTX_SIZE=4096"]))
            .is_empty());
    }

    #[test]
    fn validate_args_catches_a_bad_enum_value() {
        let caps = caps();
        // Real mistake from a model card: `--spec-type draft` for what this
        // build calls `draft-dflash`.
        let problems = caps.validate_args(&v(&["--spec-type", "draft"]));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("is not one of"), "{problems:?}");
        assert!(problems[0].contains("draft-dflash"), "{problems:?}");
        assert!(caps.validate_args(&v(&["--spec-type=draft"])).len() == 1);
        // A comma-separated subset is what this flag actually takes.
        assert!(caps
            .validate_args(&v(&["--spec-type", "draft-dflash,ngram-mod"]))
            .is_empty());
        // Soft enums never reject: a layer count is valid for --spec-draft-ngl.
        assert!(caps
            .validate_args(&v(&["--spec-draft-ngl", "40"]))
            .is_empty());
    }

    #[test]
    fn validate_args_suggests_a_near_miss() {
        let caps = caps();
        let problems = caps.validate_args(&v(&["--mmprj", "/x"]));
        assert_eq!(
            problems.len(),
            1,
            "the value must not double-report: {problems:?}"
        );
        assert!(
            problems[0].contains("unknown llama-server flag"),
            "{problems:?}"
        );
        assert!(problems[0].contains("--mmproj"), "{problems:?}");
    }

    #[test]
    fn validate_args_catches_strays_and_removals() {
        let caps = caps();
        let stray = caps.validate_args(&v(&["--jinja", "oops"]));
        assert_eq!(stray.len(), 1, "{stray:?}");
        assert!(stray[0].contains("stray value `oops`"), "{stray:?}");

        let removed = caps.validate_args(&v(&["--draft-max", "4"]));
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(removed[0].contains("removed"), "{removed:?}");
        assert!(
            removed[0].contains("spec-draft-n-max"),
            "replacement advice from the help: {removed:?}"
        );

        let missing = caps.validate_args(&v(&["--mmproj", "--jinja"]));
        assert!(
            missing.iter().any(|p| p.contains("expects a value")),
            "{missing:?}"
        );
    }

    #[test]
    fn validate_pair_checks_structured_fields() {
        let caps = caps();
        assert!(caps.validate_pair("ctx-size", Some("4096")).is_none());
        assert!(caps.validate_pair("--ctx-size", Some("4096")).is_none());
        // Presets spell switches as `jinja = true`; that is not an arity error.
        assert!(caps.validate_pair("jinja", Some("true")).is_none());
        assert!(caps.validate_pair("cache-type-k", Some("q4_0")).is_none());
        assert!(caps.validate_pair("cache-type-k", Some("q4_9")).is_some());
        assert!(caps.validate_pair("spec-type", Some("draft")).is_some());
        assert!(caps.validate_pair("n-gpu-layers", Some("999")).is_none());
        let unknown = caps.validate_pair("mmprj", Some("/x")).unwrap();
        assert!(unknown.contains("--mmproj"), "{unknown}");
        assert!(caps.validate_pair("draft-max", Some("4")).is_some());
        assert!(caps.validate_pair("ctx-size", None).is_some());
    }

    #[test]
    fn managed_flags_are_dash_insensitive() {
        assert!(is_managed("--ctx-size"));
        assert!(is_managed("ctx-size"));
        assert!(is_managed("-ctx-size"));
        assert!(!is_managed("--no-warmup"));
        assert!(!is_managed("no-warmup"));
    }

    /// The parser is only as good as the help it was written against, so run
    /// it over a real image when there is one — the same throwaway container
    /// the runtime uses (§3.6), so this exercises the production invocation
    /// too. CI has neither podman nor the image, hence the clean early return.
    #[test]
    fn real_help_from_the_image_parses() {
        let out = match std::process::Command::new("podman")
            .args([
                "run",
                "--rm",
                "--entrypoint",
                "/app/llama-server",
                "localhost/llama-server-cuda:official-latest",
                "--help",
            ])
            .output()
        {
            Ok(out) if out.status.success() => out,
            _ => return, // no podman, no image, no llama-server — skip
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let caps = parse_help(&text);
        assert!(
            caps.flags.len() > 100,
            "expected the full flag vocabulary, got {}",
            caps.flags.len()
        );
        let spec = caps
            .values_for("spec-type")
            .expect("the running build documents --spec-type values");
        assert!(spec.iter().any(|v| v == "draft-dflash"), "{spec:?}");
        assert!(caps.supports("ctx-size") && caps.supports("mmproj"));
        // Everything lmgw renders from a struct field must exist in the image;
        // if this trips, a managed field is writing a preset key llama-server
        // will not accept.
        let missing: Vec<&&str> = MANAGED_FLAGS.iter().filter(|f| !caps.supports(f)).collect();
        assert!(
            missing.is_empty(),
            "managed flags unknown to the image: {missing:?}"
        );
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    /// `<0|1>` is llama.cpp's type hint, not an exhaustive list: `--poll-batch`
    /// documents `<0|1>` and defaults to 50. Treating it as a hard enum
    /// rejected a value the running build accepts.
    #[test]
    fn angle_bracket_hints_never_reject() {
        let help = "\
--poll-batch <0|1>                      use polling to wait for work (default: same as --poll)
--fit [on|off]                          whether to adjust unset arguments to fit in device memory
";
        let caps = parse_help(help);
        assert_eq!(caps.validate_pair("poll-batch", Some("50")), None);
        assert!(caps
            .validate_args(&["--poll-batch".into(), "50".into()])
            .is_empty());
        // …while a square-bracket list stays authoritative.
        assert!(caps.validate_pair("fit", Some("maybe")).is_some());
        assert_eq!(caps.validate_pair("fit", Some("on")), None);
    }

    /// A column-0 line that is not a flag used to be absorbed into the
    /// previous entry, letting a usage epilogue mark an unrelated flag as
    /// removed.
    #[test]
    fn a_trailing_epilogue_cannot_forge_a_removal() {
        let help = "\
-ngl,  --gpu-layers N                   number of layers to offload

note: the --draft argument has been removed. use --spec-draft-n-max
";
        let caps = parse_help(help);
        assert!(caps.supports("gpu-layers"));
        assert!(
            !caps.is_removed("gpu-layers"),
            "epilogue leaked into the previous entry"
        );
        assert!(caps
            .validate_args(&["--gpu-layers".into(), "99".into()])
            .is_empty());
    }
}
