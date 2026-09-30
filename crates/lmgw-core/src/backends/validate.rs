//! Validation of a build definition (container-builds design §4, §10).
//!
//! Pure functions, each refusing with a sentence that names the field, what
//! is wrong with the value and — where there is one — the limit it broke.
//! Nothing here clamps, truncates or quietly rewrites a value into a valid
//! one: normalization is limited to trimming, blank-means-unset and
//! lowercasing a SHA.
//!
//! Much of this is argv hygiene. git and podman always get argv, never a shell
//! (§10), but an operand that starts with `-` is still read as an option, a
//! URL scheme such as `ext::` runs a command, and whitespace in a ref or a
//! build arg is a second word nobody typed. Those are refused here, where the
//! owner is looking, rather than at the first run.
//!
//! The one rule that needs the database — a slug is immutable once its build
//! has a run — is enforced by `store::update_build`, atomically with the
//! write.

use super::model::{BuildEdit, BuildExtra, BuildSpec, Forge, GpuBackend, DEFAULT_CCACHE_MAX_SIZE};
use super::tags::{BASE_LEN, CFG_LEN, MAX_SLUG_LEN, TAG_MAX};

/// Check and normalize a whole definition: every field below, plus the rules
/// between fields. Returns the cleaned spec the store should write.
pub fn validate_build(spec: BuildSpec) -> Result<BuildSpec, String> {
    let slug = spec.slug.trim().to_string();
    validate_slug(&slug)?;
    let name = match spec.name.trim() {
        // A build with no name is named after its slug, which is always
        // there and always readable.
        "" => slug.clone(),
        n => n.to_string(),
    };
    if name.chars().any(char::is_control) {
        return Err(format!("the name '{name}' contains a control character"));
    }
    let repo_url = spec.repo_url.trim().to_string();
    validate_repo_url("repo_url", &repo_url)?;
    let git_ref = spec.git_ref.trim().to_string();
    validate_ref("ref", &git_ref)?;
    let extras = validate_extras(spec.extras, spec.forge)?;

    let cuda_version = blank_is_none(spec.cuda_version);
    if let Some(v) = &cuda_version {
        validate_cuda_version(v)?;
        if spec.backend != GpuBackend::Cuda {
            return Err(format!(
                "cuda_version is set to '{v}' but the GPU backend is {} — clear it, or pick \
                 the cuda backend",
                spec.backend.as_str()
            ));
        }
    }
    let arch = match spec.arch {
        None => None,
        Some(list) => {
            let list: Vec<String> = list
                .into_iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect();
            // An empty list cannot be passed to a build; it is what "auto"
            // means, and the editor shows the auto value resolved.
            if list.is_empty() {
                None
            } else {
                validate_arch(&list)?;
                Some(list)
            }
        }
    };
    let dockerfile = blank_is_none(spec.dockerfile);
    if let Some(p) = &dockerfile {
        validate_dockerfile(p)?;
    }
    let target = blank_is_none(spec.target);
    if let Some(t) = &target {
        validate_target(t)?;
    }
    if let Some(edits) = &spec.edits {
        validate_edits(edits)?;
    }
    let ccache_max_size = match spec.ccache_max_size.trim() {
        "" => DEFAULT_CCACHE_MAX_SIZE.to_string(),
        s => s.to_string(),
    };
    validate_ccache_max_size(&ccache_max_size)?;
    let cpus = blank_is_none(spec.cpus);
    if let Some(c) = &cpus {
        validate_cpus(c)?;
    }
    let build_args = normalize_build_args(&spec.build_args)?;

    Ok(BuildSpec {
        slug,
        name,
        engine: spec.engine,
        repo_url,
        forge: spec.forge,
        git_ref,
        extras,
        backend: spec.backend,
        cuda_version,
        arch,
        dockerfile,
        target,
        edits: spec.edits,
        ccache: spec.ccache,
        ccache_max_size,
        cpus,
        build_args,
        keep_layers: spec.keep_layers,
        keep_runs: spec.keep_runs,
        notes: spec.notes,
    })
}

fn blank_is_none(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn has_space_or_control(s: &str) -> bool {
    s.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ---------------------------------------------------------------------------
// Slug
// ---------------------------------------------------------------------------

/// A slug is the tag (§4): `[a-z0-9][a-z0-9._-]*`, at most
/// [`MAX_SLUG_LEN`] characters so the longest immutable tag fits
/// [`TAG_MAX`], and never shaped like `<x>-<7 hex>-<6 hex>` — that is what an
/// immutable run tag looks like, and a moving tag spelled that way would be
/// indistinguishable from (and could overwrite) another build's run.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("the slug cannot be empty — it becomes the image tag \
                    (localhost/lmgw-<engine>:<slug>)"
            .into());
    }
    let len = slug.chars().count();
    if len > MAX_SLUG_LEN {
        return Err(format!(
            "the slug '{slug}' is {len} characters; the longest a slug can be is {MAX_SLUG_LEN}, \
             because the immutable tag localhost/lmgw-llama-server:<slug>-<base7>-<cfg6> must \
             fit {TAG_MAX} characters"
        ));
    }
    let first = slug.chars().next().unwrap_or('-');
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!(
            "the slug '{slug}' must start with a lowercase letter or a digit"
        ));
    }
    if let Some(bad) = slug
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "the slug '{slug}' contains '{bad}' — a slug is an image tag: lowercase letters, \
             digits, '.', '_' and '-'"
        ));
    }
    if looks_like_run_tag(slug) {
        return Err(format!(
            "the slug '{slug}' ends in -<{BASE_LEN} hex>-<{CFG_LEN} hex>, which is how an \
             immutable run tag ends — its moving tag could collide with another build's run; \
             pick a slug that does not end that way"
        ));
    }
    Ok(())
}

fn looks_like_run_tag(slug: &str) -> bool {
    let hex = |s: &str, n: usize| {
        s.len() == n
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let mut parts = slug.rsplitn(3, '-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(cfg), Some(base), Some(rest)) => {
            !rest.is_empty() && hex(base, BASE_LEN) && hex(cfg, CFG_LEN)
        }
        _ => false,
    }
}

/// A free slug for a duplicate of `slug`: `<slug>-copy`, then `-copy-2`, `-3`,
/// …, with the base shortened if that is what it takes to stay within
/// [`MAX_SLUG_LEN`]. `taken` answers whether a slug is already in use.
pub fn copy_slug(slug: &str, taken: impl Fn(&str) -> bool) -> String {
    (1u64..)
        .map(|n| {
            let suffix = if n == 1 {
                "-copy".to_string()
            } else {
                format!("-copy-{n}")
            };
            let room = MAX_SLUG_LEN.saturating_sub(suffix.len());
            // Slugs are ASCII, so a byte cut is a character cut.
            let base = &slug[..slug.len().min(room)];
            format!("{base}{suffix}")
        })
        .find(|candidate| !taken(candidate))
        .expect("an unbounded sequence of distinct candidates has one that is free")
}

// ---------------------------------------------------------------------------
// URLs and refs
// ---------------------------------------------------------------------------

/// The URL forms a build may name (§10): `https://`, `http://`, `ssh://`,
/// `git@host:path` and `file://` (a local mirror, and what the tests use).
/// Anything else — `ext::` (runs a command), `fd::`, a bare path that git
/// would take for a local repository — is refused.
pub fn validate_repo_url(field: &str, url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err(format!("{field} cannot be empty"));
    }
    if url.starts_with('-') {
        return Err(format!(
            "{field} '{url}' starts with '-', which git would read as an option"
        ));
    }
    if has_space_or_control(url) {
        return Err(format!(
            "{field} '{url}' contains whitespace or a control character"
        ));
    }
    let accepted = "use https://, http://, ssh://, git@host:path or file://";
    for scheme in ["https://", "http://", "ssh://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            if let Some(userinfo) = userinfo(rest) {
                // A token in the URL would be stored in the build, stamped on
                // every image as a label, echoed into the run log and shown
                // in git's errors. The forge token setting sends it only to
                // its own host, in a header, and never writes it anywhere.
                // ssh:// may name its user (`ssh://git@host/…`); a password
                // there, or any userinfo over HTTP(S), is refused.
                if scheme != "ssh://" || userinfo.contains(':') {
                    return Err(format!(
                        "{field} '{}' carries credentials in the URL — remove them and add a \
                         forge token for {} under Settings → Backends instead (lmgw sends it \
                         only to that host and never stores it in the build, its images or \
                         the run log)",
                        redact_url(url),
                        authority_host(rest).unwrap_or_else(|| "its host".into())
                    ));
                }
            }
            if authority_host(rest).is_none() {
                return Err(format!("{field} '{url}' names no host"));
            }
            return Ok(());
        }
    }
    if let Some(path) = url.strip_prefix("file://") {
        if path.is_empty() {
            return Err(format!("{field} '{url}' names no path"));
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("git@") {
        return match rest.split_once(':') {
            Some((host, path)) if !host.is_empty() && !host.contains('/') && !path.is_empty() => {
                Ok(())
            }
            _ => Err(format!(
                "{field} '{url}' is not a git@host:path address — {accepted}"
            )),
        };
    }
    Err(format!(
        "{field} '{url}' is not a git URL lmgw accepts — {accepted}"
    ))
}

/// The `user[:password]` of an authority (`user@host/…`), if it has one.
fn userinfo(rest: &str) -> Option<&str> {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit_once('@').map(|(u, _)| u)
}

/// `url` with the password of its userinfo — and, over HTTP(S), the whole
/// userinfo, which there is always a credential — replaced by `***`: what may
/// go into a log line, an error or an image label. [`validate_repo_url`]
/// refuses such URLs; this is the second line, for anything that reaches a
/// message without having passed it.
pub fn redact_url(url: &str) -> String {
    for scheme in ["https://", "http://", "ssh://"] {
        let Some(rest) = url.strip_prefix(scheme) else {
            continue;
        };
        let Some(info) = userinfo(rest) else {
            return url.to_string();
        };
        let after = &rest[info.len()..];
        let shown = match (scheme, info.split_once(':')) {
            ("ssh://", None) => info.to_string(),
            ("ssh://", Some((user, _))) => format!("{user}:***"),
            _ => "***".to_string(),
        };
        return format!("{scheme}{shown}{after}");
    }
    url.to_string()
}

/// `host[:port]` of an authority (`user@host:port/path…`), lowercased.
fn authority_host(rest: &str) -> Option<String> {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    (!host.is_empty() && !host.starts_with(':')).then(|| host.to_ascii_lowercase())
}

/// The host a repository URL points at — `github.com`, `git.example.com`,
/// `git.example:8443` — which is what forge tokens are keyed by (§7). `None`
/// for `file://` and anything that is not a URL [`validate_repo_url`] accepts.
pub fn repo_host(url: &str) -> Option<String> {
    for scheme in ["https://", "http://", "ssh://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return authority_host(rest);
        }
    }
    let rest = url.strip_prefix("git@")?;
    let (host, _) = rest.split_once(':')?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The forge a new build gets when none was chosen (§4): `github` for
/// github.com, `gitlab` for a host a forge token is configured for, `plain`
/// otherwise.
pub fn default_forge(repo_url: &str, has_token: impl Fn(&str) -> bool) -> Forge {
    match repo_host(repo_url) {
        Some(h) if h == "github.com" => Forge::Github,
        Some(h) if has_token(&h) => Forge::Gitlab,
        _ => Forge::Plain,
    }
}

/// A branch, tag, commit or full ref (`refs/pull/123/head`), checked the way
/// `git check-ref-format` would, roughly — plus the argv rules (§10).
pub fn validate_ref(field: &str, r: &str) -> Result<(), String> {
    if r.is_empty() {
        return Err(format!(
            "{field} cannot be empty — name a branch, tag or commit"
        ));
    }
    if r.starts_with('-') {
        return Err(format!(
            "{field} '{r}' starts with '-', which git would read as an option"
        ));
    }
    if has_space_or_control(r) {
        return Err(format!(
            "{field} '{r}' contains whitespace or a control character"
        ));
    }
    if let Some(bad) = r.chars().find(|c| "~^:?*[\\".contains(*c)) {
        return Err(format!(
            "{field} '{r}' contains '{bad}', which git does not allow in a ref name"
        ));
    }
    if r == "@" || r.contains("@{") {
        return Err(format!(
            "{field} '{r}' uses '@' reflog syntax, which is not a ref name"
        ));
    }
    if r.contains("..") {
        return Err(format!(
            "{field} '{r}' contains '..', which git does not allow in a ref name"
        ));
    }
    if r.starts_with('/') || r.ends_with('/') || r.contains("//") {
        return Err(format!(
            "{field} '{r}' has an empty path component (a leading, trailing or doubled '/')"
        ));
    }
    if r.ends_with('.') {
        return Err(format!("{field} '{r}' cannot end with '.'"));
    }
    for component in r.split('/') {
        if component.starts_with('.') || component.ends_with(".lock") {
            return Err(format!(
                "{field} '{r}': the component '{component}' starts with '.' or ends with \
                 '.lock', which git does not allow"
            ));
        }
    }
    Ok(())
}

/// A pin names exactly one commit, so it is a full SHA: 40 hex characters,
/// or 64 in a SHA-256 repository. Returned lowercased.
pub fn validate_pin(field: &str, pin: &str) -> Result<String, String> {
    let pin = pin.trim().to_ascii_lowercase();
    if !(pin.len() == 40 || pin.len() == 64) || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{field} '{pin}' is not a full commit SHA — a pin builds exactly one commit, so it \
             is 40 hex characters (64 in a SHA-256 repository)"
        ));
    }
    Ok(pin)
}

/// The extras list (§4): each entry checked, pins lowercased, no entry twice,
/// and no `pr` entry on a `plain` forge — without a forge there is no
/// `refs/pull/N/head` to resolve it by.
pub fn validate_extras(extras: Vec<BuildExtra>, forge: Forge) -> Result<Vec<BuildExtra>, String> {
    let mut out: Vec<BuildExtra> = Vec::with_capacity(extras.len());
    for (i, extra) in extras.into_iter().enumerate() {
        let at = format!("extras[{i}]");
        let clean = match extra {
            BuildExtra::Pr { number, pin } => {
                if number == 0 {
                    return Err(format!("{at}: a PR number starts at 1"));
                }
                if forge == Forge::Plain {
                    return Err(format!(
                        "{at}: PR #{number} needs a forge to resolve it (github or gitlab); on a \
                         plain repository add it as a ref extra instead, e.g. refs/pull/{number}/head"
                    ));
                }
                BuildExtra::Pr {
                    number,
                    pin: pin
                        .map(|p| validate_pin(&format!("{at}.pin"), &p))
                        .transpose()?,
                }
            }
            BuildExtra::Ref {
                remote_url,
                git_ref,
                pin,
            } => {
                let remote_url = remote_url.trim().to_string();
                validate_repo_url(&format!("{at}.remote_url"), &remote_url)?;
                let git_ref = git_ref.trim().to_string();
                validate_ref(&format!("{at}.ref"), &git_ref)?;
                BuildExtra::Ref {
                    remote_url,
                    git_ref,
                    pin: pin
                        .map(|p| validate_pin(&format!("{at}.pin"), &p))
                        .transpose()?,
                }
            }
        };
        let same = |a: &BuildExtra, b: &BuildExtra| match (a, b) {
            (BuildExtra::Pr { number: x, .. }, BuildExtra::Pr { number: y, .. }) => x == y,
            (
                BuildExtra::Ref {
                    remote_url: u1,
                    git_ref: r1,
                    ..
                },
                BuildExtra::Ref {
                    remote_url: u2,
                    git_ref: r2,
                    ..
                },
            ) => u1 == u2 && r1 == r2,
            _ => false,
        };
        if let Some(j) = out.iter().position(|o| same(o, &clean)) {
            return Err(format!(
                "{at}: {} is already extras[{j}] — an extra is merged once",
                clean.label()
            ));
        }
        out.push(clean);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Build knobs
// ---------------------------------------------------------------------------

/// Build args lmgw passes itself, each with the field that owns it. A second
/// `--build-arg` for the same key would silently win or lose depending on
/// argv order; refusing it names the field to use instead. Every name
/// [`super::presets::build_args`] can pass is here.
const RESERVED_BUILD_ARGS: [(&str, &str); 16] = [
    ("CUDA_VERSION", "the cuda_version field"),
    ("CUDA_DOCKER_ARCH", "the arch field"),
    ("CUDA_ARCHITECTURES", "the arch field"),
    ("ROCM_DOCKER_ARCH", "the arch field"),
    ("APP_VERSION", DERIVED),
    ("APP_REVISION", DERIVED),
    (
        "BUILD_DATE",
        "nothing — lmgw sets it to the commit date, so the layer cache survives",
    ),
    ("IMAGE_URL", "the repo_url field"),
    ("IMAGE_SOURCE", "the repo_url field"),
    ("LLAMA_BUILD_NUMBER", DERIVED),
    ("LLAMA_BUILD_COMMIT", DERIVED),
    ("AUDIOCPP_VERSION", DERIVED),
    ("AUDIOCPP_GIT_SHA", DERIVED),
    ("AUDIOCPP_GIT_DATE", DERIVED),
    ("SDCPP_BUILD_VERSION", DERIVED),
    ("SDCPP_BUILD_COMMIT", DERIVED),
];

const DERIVED: &str = "nothing — lmgw derives it from the commit it builds";

/// `KEY=VALUE` per line, blank lines ignored, key and value trimmed. A key is
/// `[A-Za-z_][A-Za-z0-9_]*`, set once, and not one lmgw passes itself.
pub fn parse_build_args(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = n + 1;
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("build_args line {lineno} ('{line}') is not KEY=VALUE"))?;
        let (k, v) = (k.trim(), v.trim());
        let valid_key = k
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            return Err(format!(
                "build_args line {lineno}: '{k}' is not a build-arg name (letters, digits and \
                 '_', not starting with a digit)"
            ));
        }
        if v.chars().any(char::is_control) {
            return Err(format!(
                "build_args line {lineno}: the value of {k} contains a control character"
            ));
        }
        if let Some((_, owner)) = RESERVED_BUILD_ARGS.iter().find(|(r, _)| *r == k) {
            return Err(format!(
                "build_args line {lineno}: {k} is passed by lmgw itself — set it through {owner}"
            ));
        }
        if out.iter().any(|(seen, _)| seen == k) {
            return Err(format!(
                "build_args line {lineno}: {k} is set twice — a build arg takes one value"
            ));
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// [`parse_build_args`], written back one `KEY=VALUE` per line — what the
/// store keeps, so a retyped list with stray blanks is not a change.
pub fn normalize_build_args(text: &str) -> Result<String, String> {
    Ok(parse_build_args(text)?
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// A `--cpuset-cpus` list: comma-separated CPU numbers and `N-M` ranges
/// (`0-15`, `0,2,4`, `0-7,16-23`).
pub fn validate_cpus(s: &str) -> Result<(), String> {
    let bad = |why: &str| {
        Err(format!(
            "cpus '{s}' {why} — use CPU numbers and ranges separated by commas, e.g. 0-15 or \
             0,2,4 (leave it empty for all cores)"
        ))
    };
    let num = |p: &str| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u32>().ok())
            .flatten()
    };
    for item in s.split(',') {
        match item.split_once('-') {
            None => {
                if num(item).is_none() {
                    return bad(&format!("has '{item}', which is not a CPU number"));
                }
            }
            Some((lo, hi)) => match (num(lo), num(hi)) {
                (Some(lo), Some(hi)) if lo <= hi => {}
                (Some(_), Some(_)) => {
                    return bad(&format!("has the range '{item}', which runs backwards"))
                }
                _ => return bad(&format!("has '{item}', which is not a CPU range")),
            },
        }
    }
    Ok(())
}

/// `CCACHE_MAXSIZE` as ccache reads it: a number with an optional decimal
/// part and an optional suffix — `k`, `M`, `G`, `T` (decimal) or `Ki`, `Mi`,
/// `Gi`, `Ti` (binary); a bare number is gigabytes, and `0` is no limit.
pub fn validate_ccache_max_size(s: &str) -> Result<(), String> {
    let digits_end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(digits_end);
    let number_ok = !number.is_empty()
        && !number.starts_with('.')
        && !number.ends_with('.')
        && number.matches('.').count() <= 1;
    let suffix_ok = matches!(
        suffix,
        "" | "k" | "M" | "G" | "T" | "Ki" | "Mi" | "Gi" | "Ti"
    );
    if !(number_ok && suffix_ok) {
        return Err(format!(
            "ccache_max_size '{s}' is not a size ccache reads — a number with an optional k, M, \
             G, T (or Ki, Mi, Gi, Ti) suffix, e.g. 10G or 500M; 0 means no limit"
        ));
    }
    Ok(())
}

/// A CUDA toolkit version as the upstream Dockerfiles take it: `13.0.0`,
/// `12.8.1`, `12.4`.
pub fn validate_cuda_version(v: &str) -> Result<(), String> {
    let parts: Vec<&str> = v.split('.').collect();
    let ok = (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        return Err(format!(
            "cuda_version '{v}' is not a CUDA version — use major.minor[.patch], e.g. 13.0.0"
        ));
    }
    Ok(())
}

/// A GPU architecture list, one entry per architecture: CUDA compute
/// capabilities (`89`, `90a`, `86-real`) or ROCm targets (`gfx1100`).
pub fn validate_arch(list: &[String]) -> Result<(), String> {
    for (i, a) in list.iter().enumerate() {
        if a.contains([';', ',', ' ']) {
            return Err(format!(
                "arch[{i}] '{a}' holds more than one architecture — give one per entry"
            ));
        }
        let ok = a
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !ok {
            return Err(format!(
                "arch[{i}] '{a}' is not an architecture name — e.g. 89 (a CUDA compute \
                 capability) or gfx1100 (a ROCm target)"
            ));
        }
        if list[..i].contains(a) {
            return Err(format!("arch[{i}] '{a}' is listed twice"));
        }
    }
    Ok(())
}

/// A Dockerfile path relative to the repository root that stays inside it.
pub fn validate_dockerfile(p: &str) -> Result<(), String> {
    if p.starts_with('-') || has_space_or_control(p) || p.contains('\\') {
        return Err(format!(
            "dockerfile '{p}' must be a plain repository path — no leading '-', whitespace or \
             backslashes"
        ));
    }
    if p.starts_with('/') {
        return Err(format!(
            "dockerfile '{p}' is absolute; it is a path inside the repository, e.g. \
             .devops/cuda.Dockerfile"
        ));
    }
    if p.split('/').any(|c| c == "..") {
        return Err(format!(
            "dockerfile '{p}' climbs out of the repository with '..'"
        ));
    }
    Ok(())
}

/// A build stage name (`server`, `runtime`, `full`).
pub fn validate_target(t: &str) -> Result<(), String> {
    let ok = t.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "target '{t}' is not a build stage name — letters, digits, '.', '_' and '-', \
             starting with a letter or digit"
        ));
    }
    Ok(())
}

/// Dockerfile edits: each one has something to find. `find` and `replace` may
/// span lines (a `RUN` with continuations), so newlines and tabs are fine; a
/// NUL is not text.
pub fn validate_edits(edits: &[BuildEdit]) -> Result<(), String> {
    for (i, e) in edits.iter().enumerate() {
        if e.name.chars().any(char::is_control) {
            return Err(format!(
                "edits[{i}]: the name '{}' contains a control character",
                e.name.escape_debug()
            ));
        }
        if e.find.is_empty() {
            return Err(format!(
                "edits[{i}]: find is empty — an edit replaces text it finds"
            ));
        }
        if e.find.contains('\0') || e.replace.contains('\0') {
            return Err(format!("edits[{i}] contains a NUL character"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Forge tokens (settings)
// ---------------------------------------------------------------------------

/// A forge token's host key (§7): `github.com`, `git.example.com`,
/// `git.example:8443`. Lowercased, because a host name is case-insensitive,
/// and without a default port (`:443`, `:80`), because the lookup side
/// ([`crate::backends::forge::token_host`]) never produces one — a token
/// saved under `git.example:443` would otherwise never be found.
pub fn validate_forge_host(raw: &str) -> Result<String, String> {
    let h = validate_forge_host_as_typed(raw)?;
    Ok(match h.rsplit_once(':') {
        Some((name, "443" | "80")) => name.to_string(),
        _ => h,
    })
}

fn validate_forge_host_as_typed(raw: &str) -> Result<String, String> {
    let h = raw.trim().to_ascii_lowercase();
    let (name, port) = match h.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h.as_str(), None),
    };
    let name_ok = !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0));
    if !name_ok || !port_ok {
        return Err(format!(
            "forge token host '{raw}' is not a host name — the host alone, e.g. github.com or \
             git.example:8443 (no scheme, no path)"
        ));
    }
    Ok(h)
}

/// A forge token goes into an HTTP header and a git `extraHeader`: whitespace
/// or a control character in it would end the header early.
pub fn validate_forge_token(host: &str, token: &str) -> Result<(), String> {
    if token.is_empty() || has_space_or_control(token) {
        return Err(format!(
            "the forge token for {host} is empty or contains whitespace or a control character"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::model::Engine;

    fn spec() -> BuildSpec {
        BuildSpec {
            slug: "official-master".into(),
            name: "Official master".into(),
            engine: Engine::Llama,
            repo_url: "https://github.com/ggml-org/llama.cpp".into(),
            forge: Forge::Github,
            git_ref: "master".into(),
            ..BuildSpec::default()
        }
    }

    #[test]
    fn slugs_use_the_tag_charset() {
        for ok in [
            "official-master",
            "ik.main",
            "a",
            "0",
            "sd_cpp-2",
            "master-pr16391",
        ] {
            assert_eq!(validate_slug(ok), Ok(()), "{ok}");
        }
        for bad in ["", "Official", "-x", ".x", "_x", "a b", "a/b", "a:b", "ä"] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_slug_limit_is_named_in_the_refusal() {
        assert_eq!(validate_slug(&"a".repeat(MAX_SLUG_LEN)), Ok(()));
        let err = validate_slug(&"a".repeat(MAX_SLUG_LEN + 1)).unwrap_err();
        assert!(err.contains("85"), "{err}");
        assert!(err.contains("128"), "{err}");
    }

    #[test]
    fn a_slug_shaped_like_a_run_tag_is_refused() {
        let err = validate_slug("master-4b1a27f-abcdef").unwrap_err();
        assert!(err.contains("immutable run tag"), "{err}");
        // Close, but not that shape.
        assert_eq!(validate_slug("master-4b1a27f"), Ok(()));
        assert_eq!(validate_slug("4b1a27f-abcdef"), Ok(()));
        assert_eq!(validate_slug("master-pr16391-abcdef"), Ok(()));
        assert_eq!(validate_slug("master-4b1a27f-abcdeg"), Ok(()));
    }

    #[test]
    fn a_copy_slug_is_free_and_within_the_limit() {
        let taken = ["x-copy", "x-copy-2"];
        assert_eq!(copy_slug("x", |s| taken.contains(&s)), "x-copy-3");
        assert_eq!(copy_slug("x", |_| false), "x-copy");
        let long = "a".repeat(MAX_SLUG_LEN);
        let c = copy_slug(&long, |_| false);
        assert_eq!(c.len(), MAX_SLUG_LEN);
        assert!(c.ends_with("-copy"));
        assert_eq!(validate_slug(&c), Ok(()));
    }

    #[test]
    fn only_known_url_schemes_are_accepted() {
        for ok in [
            "https://github.com/ggml-org/llama.cpp",
            "http://git.local/x.git",
            "ssh://git@git.example.com:2222/p/x.git",
            "git@github.com:0xShug0/audio.cpp.git",
            "file:///srv/mirror/llama.cpp",
        ] {
            assert_eq!(validate_repo_url("repo_url", ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "-uhttps://x",
            "ext::sh -c touch% /tmp/pwned",
            "fd::3",
            "/srv/mirror/llama.cpp",
            "github.com/ggml-org/llama.cpp",
            "https://",
            "https:///path",
            "https://github.com/a b",
            "git@:path",
            "git@host",
            "file://",
        ] {
            assert!(validate_repo_url("repo_url", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn credentials_in_a_repo_url_are_refused_and_never_repeated() {
        for bad in [
            "https://ghp_secret@github.com/o/r",
            "https://x-access-token:ghp_secret@github.com/o/r",
            "http://oauth2:glpat-secret@git.local/o/r.git",
            "ssh://git:hunter2@git.example.com:2222/p/x.git",
        ] {
            let e = validate_repo_url("repo_url", bad).unwrap_err();
            assert!(e.contains("credentials"), "{e}");
            assert!(e.contains("Settings → Backends"), "{e}");
            assert!(!e.contains("secret") && !e.contains("hunter2"), "{e}");
        }
        // A user name alone is how ssh:// works — not a credential.
        assert_eq!(
            validate_repo_url("repo_url", "ssh://git@git.example.com/p/x.git"),
            Ok(())
        );
        assert_eq!(
            redact_url("https://x-access-token:ghp_secret@github.com/o/r"),
            "https://***@github.com/o/r"
        );
        assert_eq!(
            redact_url("ssh://git:hunter2@host/p"),
            "ssh://git:***@host/p"
        );
        assert_eq!(redact_url("ssh://git@host/p"), "ssh://git@host/p");
        assert_eq!(
            redact_url("https://github.com/o/r@x"),
            "https://github.com/o/r@x",
            "an @ in the path is not userinfo"
        );
        assert_eq!(redact_url("git@github.com:o/r"), "git@github.com:o/r");
    }

    #[test]
    fn the_host_is_what_forge_tokens_are_keyed_by() {
        assert_eq!(
            repo_host("https://GitHub.com/ggml-org/llama.cpp").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            repo_host("https://user:pw@git.example:8443/x").as_deref(),
            Some("git.example:8443")
        );
        assert_eq!(
            repo_host("git@git.example.com:p/x.git").as_deref(),
            Some("git.example.com")
        );
        assert_eq!(repo_host("file:///srv/x"), None);
        assert_eq!(
            default_forge("https://github.com/a/b", |_| false),
            Forge::Github
        );
        assert_eq!(
            default_forge("https://git.example.com/a/b", |h| h == "git.example.com"),
            Forge::Gitlab
        );
        assert_eq!(
            default_forge("https://git.example.com/a/b", |_| false),
            Forge::Plain
        );
    }

    #[test]
    fn refs_follow_git_check_ref_format_roughly() {
        for ok in [
            "master",
            "main",
            "b6000",
            "refs/pull/16391/head",
            "refs/merge-requests/12/head",
            "feature/x-y_z.1",
            "4b1a27fa0e4c1d2b3a4958677a8b9c0d1e2f3a4b",
        ] {
            assert_eq!(validate_ref("ref", ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "--upload-pack=x",
            "a b",
            "a\tb",
            "a..b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "@",
            "a@{1}",
            "/a",
            "a/",
            "a//b",
            "a.",
            ".a",
            "a/.b",
            "a.lock",
            "a/b.lock/c",
        ] {
            assert!(validate_ref("ref", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_pin_is_a_full_sha() {
        assert_eq!(
            validate_pin("pin", &"A".repeat(40)).unwrap(),
            "a".repeat(40)
        );
        assert!(validate_pin("pin", &"a".repeat(64)).is_ok());
        assert!(validate_pin("pin", "4b1a27f").is_err());
        assert!(validate_pin("pin", &"g".repeat(40)).is_err());
    }

    #[test]
    fn build_args_are_key_value_lines_with_no_reserved_or_repeated_keys() {
        assert_eq!(
            parse_build_args("\n GGML_CUDA_FA_ALL_QUANTS = ON \n\nX_1=a=b\nEMPTY=\n").unwrap(),
            vec![
                ("GGML_CUDA_FA_ALL_QUANTS".to_string(), "ON".to_string()),
                ("X_1".to_string(), "a=b".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
        assert_eq!(normalize_build_args(" A=1 \n\n B=2\n").unwrap(), "A=1\nB=2");
        assert!(parse_build_args("NOPE").is_err());
        assert!(parse_build_args("1A=x").is_err());
        assert!(parse_build_args("A-B=x").is_err());
        assert!(parse_build_args("=x").is_err());
        assert!(parse_build_args("A=1\nA=2").unwrap_err().contains("twice"));
        let err = parse_build_args("CUDA_VERSION=12.4").unwrap_err();
        assert!(err.contains("cuda_version field"), "{err}");
        let err = parse_build_args("CUDA_DOCKER_ARCH=89").unwrap_err();
        assert!(err.contains("arch field"), "{err}");
    }

    #[test]
    fn cpus_are_numbers_and_forward_ranges() {
        for ok in ["0", "0-15", "0,2,4", "0-7,16-23", "3-3"] {
            assert_eq!(validate_cpus(ok), Ok(()), "{ok}");
        }
        for bad in ["", "a", "0-", "-3", "15-0", "0,,2", "0 - 3", "0;1", "1.5"] {
            assert!(validate_cpus(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ccache_sizes_are_what_ccache_reads() {
        for ok in ["10G", "500M", "0", "5", "1.5G", "64Ki", "2Ti", "100k"] {
            assert_eq!(validate_ccache_max_size(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "G", "10 G", "10GB", "10g", ".5G", "5.G", "1.2.3G", "-1G",
        ] {
            assert!(validate_ccache_max_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_small_fields_are_checked_by_shape() {
        assert!(validate_cuda_version("13.0.0").is_ok());
        assert!(validate_cuda_version("12.4").is_ok());
        assert!(validate_cuda_version("13").is_err());
        assert!(validate_cuda_version("13.0.0-devel").is_err());

        assert!(validate_arch(&["89".into(), "90a".into(), "gfx1100".into()]).is_ok());
        assert!(validate_arch(&["86;89".into()]).is_err());
        assert!(validate_arch(&["89".into(), "89".into()]).is_err());
        assert!(validate_arch(&["-89".into()]).is_err());

        assert!(validate_dockerfile(".devops/cuda.Dockerfile").is_ok());
        assert!(validate_dockerfile("/etc/passwd").is_err());
        assert!(validate_dockerfile("../x").is_err());
        assert!(validate_dockerfile("-f").is_err());

        assert!(validate_target("server").is_ok());
        assert!(validate_target("-x").is_err());
        assert!(validate_target("a b").is_err());

        assert!(validate_edits(&[BuildEdit {
            find: "RUN a \\\n  && b".into(),
            replace: String::new(),
            required: true,
            ..BuildEdit::default()
        }])
        .is_ok());
        assert!(validate_edits(&[BuildEdit::default()]).is_err());
    }

    #[test]
    fn a_whole_spec_is_trimmed_and_blank_means_unset() {
        let mut s = spec();
        s.slug = "  official-master ".into();
        s.name = "  ".into();
        s.cuda_version = Some(" ".into());
        s.arch = Some(vec![" ".into()]);
        s.dockerfile = Some(String::new());
        s.cpus = Some("".into());
        s.ccache_max_size = " ".into();
        s.build_args = "A=1\n\n".into();
        let v = validate_build(s).unwrap();
        assert_eq!(v.slug, "official-master");
        assert_eq!(
            v.name, "official-master",
            "a nameless build is named after its slug"
        );
        assert_eq!(v.cuda_version, None);
        assert_eq!(v.arch, None);
        assert_eq!(v.dockerfile, None);
        assert_eq!(v.cpus, None);
        assert_eq!(v.ccache_max_size, "10G");
        assert_eq!(v.build_args, "A=1");
    }

    #[test]
    fn the_rules_between_fields_hold() {
        let mut s = spec();
        s.backend = GpuBackend::Vulkan;
        s.cuda_version = Some("13.0.0".into());
        assert!(validate_build(s).unwrap_err().contains("vulkan"));

        let mut s = spec();
        s.forge = Forge::Plain;
        s.extras = vec![BuildExtra::Pr {
            number: 1,
            pin: None,
        }];
        assert!(validate_build(s).unwrap_err().contains("refs/pull/1/head"));

        let mut s = spec();
        s.extras = vec![
            BuildExtra::Pr {
                number: 7,
                pin: None,
            },
            BuildExtra::Pr {
                number: 7,
                pin: Some("a".repeat(40)),
            },
        ];
        assert!(validate_build(s).unwrap_err().contains("already extras[0]"));

        let mut s = spec();
        s.extras = vec![BuildExtra::Ref {
            remote_url: "ext::sh".into(),
            git_ref: "x".into(),
            pin: None,
        }];
        assert!(validate_build(s)
            .unwrap_err()
            .contains("extras[0].remote_url"));
    }

    #[test]
    fn forge_token_hosts_are_bare_host_names() {
        assert_eq!(validate_forge_host(" GitHub.com ").unwrap(), "github.com");
        assert_eq!(
            validate_forge_host("git.example:8443").unwrap(),
            "git.example:8443"
        );
        for default_port in ["git.example:443", "GIT.example:80"] {
            assert_eq!(
                validate_forge_host(default_port).unwrap(),
                "git.example",
                "the key token_host looks the token up under"
            );
        }
        for bad in [
            "",
            "https://github.com",
            "github.com/x",
            "a..b",
            "-a.com",
            "h:0",
            "h:x",
        ] {
            assert!(validate_forge_host(bad).is_err(), "{bad}");
        }
        assert!(validate_forge_token("h", "ghp_abc").is_ok());
        assert!(validate_forge_token("h", "abc\r\nX-Evil: 1").is_err());
        assert!(validate_forge_token("h", "").is_err());
    }
}
