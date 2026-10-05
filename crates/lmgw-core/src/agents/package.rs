//! Layer 4 — the package (container-runtime design §3.4).
//!
//! An agent package is an **OCI image with the manifest inside it** at
//! [`MANIFEST_INSIDE`]. Installing one is an image reference and nothing else:
//! lmgw reads the manifest out of the image, hands it to the ordinary import
//! path — the same [`import_inner`](crate::web::api_agents) every dropped file,
//! `agent_set` and Definition-editor Save goes through, with all of its
//! validation and every warning it raises — and records on the row *where the
//! document came from*.
//!
//! **How the manifest is read: `podman create` → `podman cp` → `podman rm`.**
//! The image's filesystem through a created-but-never-started container. Three
//! invocations of the existing [`Spawner::run`] seam, no trait change, and
//! **nothing required of the image**: a `--entrypoint cat` read would need a
//! `cat` in there and would *execute* the image to read its metadata, and an
//! OCI label cannot hold kilobytes of prompt JSON legibly. Measured on podman
//! 5.8.4: `podman create` succeeds on a `FROM scratch` image with no `CMD` at
//! all, so the read really does work on an image that carries the manifest and
//! nothing else.
//!
//! `podman cp <container>:<path> -` writes a **tar stream** to stdout, which is
//! not a `String` and not something to re-implement a reader for, so the copy
//! goes to a file in a `0700` directory on the run tmpfs
//! ([`RunDir`](super::container::RunDir)) and is read back from there. The
//! directory is removed on every path by `RunDir`'s `Drop`, including a panic;
//! `podman rm -f` likewise runs on every error path, which is what
//! [`Created`]'s own `Drop` is for.
//!
//! **Nothing here invents a bound.** The pull policy is the caller's visible
//! choice (§4.1, default `never`), the manifest is read at whatever size it is,
//! and a failure names the image and the path rather than guessing.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::state::SharedState;

use super::container::{self, RunDir, Spawner};
use super::manifest::PullPolicy;
use super::Agent;

/// Where an agent package carries its manifest. A fixed path, not a
/// convention a label could redirect: "the manifest is at `/lmgw/agent.json`"
/// is the whole of the package format, and one that could be moved would need
/// a second read to find out where.
pub const MANIFEST_INSIDE: &str = "/lmgw/agent.json";

/// `podman image inspect --format '{{.Digest}}'` — the manifest digest of the
/// image as it is on this box. Present for a locally built image too, which is
/// what makes "has this tag moved?" answerable for `localhost/…`.
const DIGEST_FORMAT: &str = "{{.Digest}}";

// ---------------------------------------------------------------------------
// Provenance (§5)
// ---------------------------------------------------------------------------

/// What `agents.provenance` holds: where this row's document came from.
///
/// One JSON column (§5) rather than four, matching the house convention
/// (`manifest`, `config`, `extra_run_args` are all JSON text) and leaving room
/// for the fields a package format grows. Absent fields are empty, never
/// invented: a row installed by `agent_set` has `{}` here and says "no package"
/// rather than claiming an image nobody read.
///
/// `installed_at` and `pulled_at` are **two facts**, which is why both are
/// here: the first is when this row was written from the image, the second is
/// when the digest was last refreshed. An install sets both; `agent_pull` moves
/// only the second, so "installed in March, re-pulled yesterday" is readable
/// off the row instead of being lost to one overwritten timestamp.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provenance {
    /// The image reference **as the owner gave it**, tag and all. Not the
    /// digest-pinned form: what they typed is what an export and a re-install
    /// have to be able to repeat.
    pub image: String,
    /// `podman image inspect --format '{{.Digest}}'` at the time of the last
    /// read. Empty when podman could not say.
    pub digest: String,
    /// Where in the image the manifest was found — [`MANIFEST_INSIDE`] today,
    /// recorded so a row installed by an older build still says where it looked.
    pub manifest_path: String,
    /// RFC 3339, when this row was written from the image.
    pub installed_at: String,
    /// RFC 3339, when `digest` was last read.
    pub pulled_at: String,
}

impl Provenance {
    /// `true` when nothing was ever installed from an image here.
    pub fn is_empty(&self) -> bool {
        self.image.is_empty() && self.digest.is_empty()
    }

    /// Parse a row's column. A column this build cannot read is *no*
    /// provenance, never an error: it is a record of where something came
    /// from, and losing it must not take the agent down with it.
    pub fn of_row(row: &crate::store::AgentRow) -> Self {
        serde_json::from_str(&row.provenance).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Now, in the one format the rest of the row's timestamps use.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// The image (§3.4 step 1)
// ---------------------------------------------------------------------------

/// A failure with a **stable code** in front of it, the shape the ops plane
/// already uses (`"{message} ({code})"`, as the Start gate answers).
///
/// The three install failures are told apart by code rather than by matching on
/// a sentence: "the image is not here", "the image is not a package" and "the
/// manifest is invalid" are three different things for a caller to do something
/// about, and the third one is the import path's own error, quoted verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageError {
    pub code: &'static str,
    pub message: String,
}

impl PackageError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl From<PackageError> for String {
    fn from(e: PackageError) -> Self {
        e.to_string()
    }
}

type Result<T> = std::result::Result<T, PackageError>;

/// An image reference podman will read as a **reference** and not as a flag.
///
/// `podman image exists --help` exits `0`, so a reference of `--help` would be
/// "present" and every diagnosis after that would be about the wrong thing.
/// Every argv below also passes `--` before the positional reference (podman
/// honours it on `image exists`, `pull`, `create` and `image inspect`), so this
/// guard and that terminator are belt and braces: the guard is what produces a
/// message naming the problem instead of a parse error from podman.
fn check_ref(image: &str) -> Result<()> {
    if image.starts_with('-') {
        return Err(PackageError::new(
            "image_ref_invalid",
            format!(
                "'{image}' starts with '-', so podman would read it as a flag rather than as an \
                 image reference"
            ),
        ));
    }
    if image.is_empty() || image.chars().any(char::is_whitespace) {
        return Err(PackageError::new(
            "image_ref_invalid",
            format!("'{image}' is not an image reference: it is empty or contains whitespace"),
        ));
    }
    Ok(())
}

/// The image is on the box when this returns — or the pull policy said not to
/// fetch it and this says so, naming the image (§3.4).
///
/// `Ok(true)` means something was downloaded, which the install report prints:
/// a multi-gigabyte download is not a detail to discover from the clock.
pub async fn ensure_image(state: &SharedState, image: &str, pull: PullPolicy) -> Result<bool> {
    check_ref(image)?;
    if pull == PullPolicy::Always {
        // Whether anything was actually **downloaded** is the digest before and
        // after, not the policy: `always` on an image that has not moved copies
        // nothing, and reporting `pulled: true` for it would be lmgw inventing
        // an event. `None` before and `Some` after is an arrival.
        let before = image_digest(state, image).await;
        pull_image(state, image).await?;
        return Ok(before.is_none() || before != image_digest(state, image).await);
    }
    match container::image_present(state, image).await {
        container::ImagePresence::Present => Ok(false),
        container::ImagePresence::Absent if pull == PullPolicy::Never => Err(PackageError::new(
            "image_absent_pull_never",
            format!(
                "the image '{image}' is not on this box and the pull policy is 'never', so \
                 nothing was downloaded. Pull it yourself, or install with pull 'missing' to let \
                 lmgw fetch it."
            ),
        )),
        container::ImagePresence::Absent => {
            pull_image(state, image).await?;
            Ok(true)
        }
        container::ImagePresence::Unknown(why) => Err(PackageError::new("podman_unavailable", why)),
    }
}

/// `podman pull <image>`, with podman's own failure quoted.
///
/// **Not bounded by a timeout lmgw invented**: a pull takes as long as the
/// image is big, and a clock here would turn a slow network into a lie about
/// the registry. It ends when podman ends.
pub async fn pull_image(state: &SharedState, image: &str) -> Result<()> {
    check_ref(image)?;
    let argv = vec!["pull".to_string(), "--".to_string(), image.to_string()];
    match state.agent_spawner().run("podman", &argv).await {
        Ok(out) if out.ok() => Ok(()),
        Ok(out) => Err(PackageError::new(
            "image_pull_failed",
            format!(
                "podman pull {image} failed (exit {}): {}",
                out.status,
                excerpt(&format!("{}{}", out.stderr, out.stdout), "podman pull")
            ),
        )),
        Err(e) => Err(PackageError::new(
            "podman_unavailable",
            format!("podman pull {image} could not be run: {e}"),
        )),
    }
}

/// The image's manifest digest, or `None` when podman could not say.
///
/// `None`, never a placeholder: "lmgw does not know this image's digest" and
/// "the digest is `<unknown>`" are different facts, and only the first one is
/// true.
pub async fn image_digest(state: &SharedState, image: &str) -> Option<String> {
    if check_ref(image).is_err() {
        return None;
    }
    let argv = ["image", "inspect", "--format", DIGEST_FORMAT, "--", image]
        .map(String::from)
        .to_vec();
    match state.agent_spawner().run("podman", &argv).await {
        Ok(out) if out.ok() => {
            let d = out.stdout.trim().to_string();
            (!d.is_empty()).then_some(d)
        }
        _ => None,
    }
}

/// The tail of podman's complaint for a message that has to fit in a toast —
/// and the **whole** of it in the log.
///
/// [`container::STDERR_EXCERPT_LINES`] is the excerpt size the rest of this
/// runtime uses, not a second number invented here, and nothing is dropped:
/// what the message trims off is one `journalctl` away, because a truncated
/// diagnosis with the rest thrown away is the failure mode this design spends
/// its time avoiding.
fn excerpt(text: &str, what: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = container::STDERR_EXCERPT_LINES;
    if lines.len() > n {
        tracing::warn!("{what} failed; its full output was:\n{}", lines.join("\n"));
    }
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// ---------------------------------------------------------------------------
// create → cp → rm (§3.4)
// ---------------------------------------------------------------------------

/// A container created purely to be read, removed by `Drop` whatever happens.
///
/// The removal is the part that must not be conditional: an install that fails
/// at the copy, at the parse or at a panic in between would otherwise leave a
/// `lmgw-pkg-…` container on the box for every attempt. `Drop` cannot await, so
/// the removal is spawned — and [`remove`](Self::remove) is what the happy path
/// calls so the common case is still synchronous and observable by a test.
struct Created {
    name: String,
    spawner: Option<Arc<dyn Spawner>>,
}

impl Created {
    async fn remove(mut self) {
        if let Some(spawner) = self.spawner.take() {
            rm_f(&spawner, &self.name).await;
        }
    }
}

impl Drop for Created {
    fn drop(&mut self) {
        if let Some(spawner) = self.spawner.take() {
            let name = self.name.clone();
            tokio::spawn(async move { rm_f(&spawner, &name).await });
        }
    }
}

async fn rm_f(spawner: &Arc<dyn Spawner>, name: &str) {
    let argv = vec!["rm".to_string(), "-f".to_string(), name.to_string()];
    match spawner.run("podman", &argv).await {
        Ok(out) if out.ok() => {}
        Ok(out) => tracing::warn!(
            "removing the package container {name} failed (exit {}): {}",
            out.status,
            out.stderr.trim()
        ),
        Err(e) => tracing::warn!("removing the package container {name} could not be run: {e}"),
    }
}

/// The `lmgw.run` label the throwaway container carries.
///
/// Not a job id, deliberately — the same choice [`container::RUN_LABEL_SERVICE`]
/// makes: boot reconciliation collects every agent container whose `lmgw.run`
/// is not a **live job row**, and a package read has no job at all, so
/// `"package"` failing `parse::<i64>()` is the right answer.
pub const RUN_LABEL_PACKAGE: &str = "package";

/// What a package read's run directory is called, so
/// [`container::reconcile()`]'s sweep can recognise one.
pub const RUN_DIR_PREFIX: &str = "pkg-";

/// `lmgw-pkg-<rand>` — the throwaway container's name (§3.4).
///
/// Random rather than derived from the image: two installs of the same image at
/// once must not collide, and the container lives for three invocations.
pub fn package_container_name() -> String {
    let n: u64 = rand::random();
    format!("lmgw-pkg-{n:016x}")
}

/// The manifest text carried by an image, read without starting it.
///
/// The image must already be on the box — [`ensure_image`] is the step that
/// makes sure of it, kept separate so "it is not here and you said never" is
/// its own answer rather than a create that fails for an unrelated-looking
/// reason.
pub async fn read_manifest(state: &SharedState, image: &str) -> Result<String> {
    check_ref(image)?;
    let spawner = state.agent_spawner();
    let name = package_container_name();
    let prefix = state.snapshot().settings.container_prefix.clone();
    // **Labelled like every other container this instance makes** (§6.4): a
    // SIGKILL between the `create` and the `rm -f` would otherwise leave a
    // container boot reconciliation cannot see, because it collects by
    // `lmgw.kind=agent` + `lmgw.instance=<prefix>`. `lmgw.run=package` is not a
    // job id, which is exactly the "collect it" answer the reconciler gives a
    // run label it cannot parse — the same reading `service` gets.
    // `--pull=never`: `ensure_image` has already settled the download question
    // and said so out loud. A create that could quietly fetch would make the
    // visible pull policy a suggestion.
    let create = vec![
        "create".to_string(),
        "--pull=never".to_string(),
        "--name".to_string(),
        name.clone(),
        "--label".to_string(),
        format!("{}={prefix}", container::LABEL_INSTANCE),
        "--label".to_string(),
        format!("{}={}", container::LABEL_KIND, container::KIND_AGENT),
        "--label".to_string(),
        format!("{}={RUN_LABEL_PACKAGE}", container::LABEL_RUN),
        "--".to_string(),
        image.to_string(),
    ];
    match spawner.run("podman", &create).await {
        Ok(out) if out.ok() => {}
        Ok(out) => {
            return Err(PackageError::new(
                "package_create_failed",
                format!(
                    "podman create from '{image}' failed (exit {}): {}",
                    out.status,
                    excerpt(&out.stderr, "podman create")
                ),
            ))
        }
        Err(e) => {
            return Err(PackageError::new(
                "podman_unavailable",
                format!("podman create from '{image}' could not be run: {e}"),
            ))
        }
    }
    let created = Created {
        name: name.clone(),
        spawner: Some(spawner.clone()),
    };

    // `podman cp <c>:<path> -` is a **tar stream**; a file on the run tmpfs is
    // what this reads instead, in a 0700 directory that goes away with its
    // `RunDir` — on the error paths and on a panic too.
    let (root, _) = container::runs_root(&state.data_dir, &prefix);
    let dir = match RunDir::create_named(&root, &format!("{RUN_DIR_PREFIX}{name}")) {
        Ok(d) => d,
        Err(e) => {
            created.remove().await;
            return Err(PackageError::new("package_read_failed", e));
        }
    };
    let out_path = dir.path().join("agent.json");
    let cp = vec![
        "cp".to_string(),
        format!("{name}:{MANIFEST_INSIDE}"),
        out_path.to_string_lossy().into_owned(),
    ];
    let copied = spawner.run("podman", &cp).await;
    let text = match copied {
        Ok(out) if out.ok() => std::fs::read_to_string(&out_path).map_err(|e| {
            PackageError::new(
                "package_read_failed",
                format!("the manifest copied out of '{image}' could not be read: {e}"),
            )
        }),
        // podman says "could not be found on container … no such file or
        // directory" and exits 125. The sentence §3.4 asks for is lmgw's, with
        // podman's quoted after it so a permission or a path typo is still
        // legible.
        Ok(out) => Err(PackageError::new(
            "package_no_manifest",
            format!(
                "the image '{image}' carries no {MANIFEST_INSIDE}; an lmgw agent package puts \
                 its manifest there. podman said: {}",
                excerpt(&out.stderr, "podman cp")
            ),
        )),
        Err(e) => Err(PackageError::new(
            "podman_unavailable",
            format!("podman cp from '{image}' could not be run: {e}"),
        )),
    };
    created.remove().await;
    text
}

// ---------------------------------------------------------------------------
// Which image an agent's package is
// ---------------------------------------------------------------------------

/// The image `agent_pull` and `agent_reimport` act on.
///
/// **The manifest's `run.image` wins**, because that is the image the agent
/// actually runs (WP2): pulling anything else would refresh a digest for an
/// image no phase ever starts. `provenance.image` is the fallback for the one
/// row that has no `run.image` — a service agent served from a `dev_url` — and
/// the historical record otherwise. When the two disagree the Run tab says so
/// (`install_image_mismatch`) rather than picking silently.
pub fn image_of(agent: &Agent) -> Option<String> {
    if let Some(image) = agent.manifest.image() {
        return Some(image.to_string());
    }
    let p = Provenance::of_row(&agent.row);
    (!p.image.is_empty()).then_some(p.image)
}

// ---------------------------------------------------------------------------
// Export: the portability line (§3.4, §8)
// ---------------------------------------------------------------------------

/// Can the file this export produces be installed on another box, and if not,
/// what would the receiver have to do?
///
/// **Said, never enforced.** Exporting to the same box is the common case and a
/// refusal would be wrong; what would be worse is a file that looks complete
/// and names an image only the exporter has. Both facts that can make it
/// unportable are here:
///
/// - a `localhost/…` image (or one with no registry host at all), which is
///   local to the machine that built it, and
/// - a `dev_url`, which is not in the file at all — the export is the manifest,
///   and the manifest has no app to serve without either an image or a dev
///   server on the receiving box.
///
/// **`redact_dev_url` is what keeps the URL out of the file.** The note has to
/// exist in both places — the fact that this agent only works here because of
/// something the file does not contain is exactly what the line is for — but
/// the *address of a server on the exporting machine* is not the receiver's
/// business, and an export is a file that gets mailed around. So the export
/// says that there is one and the dashboard, which is showing the owner their
/// own row, names it.
pub fn portability(agent: &Agent, redact_dev_url: bool) -> (bool, Vec<String>) {
    let mut notes = Vec::new();
    if let Some(image) = agent.manifest.image() {
        if container::is_local_image(image) {
            notes.push(format!(
                "the image '{image}' is local to the machine that built it; the receiver must \
                 build or retag it"
            ));
        }
    }
    if let Some(url) = super::service::dev_url_of(agent) {
        notes.push(if redact_dev_url {
            "this agent's app is currently served from a dev server on the exporting machine. A \
             dev_url is a row setting and is never exported — neither the override nor its \
             address — so the receiver gets the image path only"
                .to_string()
        } else {
            format!(
                "this agent's app is currently served from the dev server at {url}. A dev_url is \
                 a row setting and is never exported, so the receiver gets the image path only"
            )
        });
    }
    if agent.manifest.kind() == "container" && agent.manifest.image().is_none() {
        notes.push(
            "this manifest names no run.image, so the receiver has nothing to run it with"
                .to_string(),
        );
    }
    (notes.is_empty(), notes)
}

#[cfg(test)]
mod tests;
