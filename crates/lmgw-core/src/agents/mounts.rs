//! Where a bound mount may point (mounts design §5.3).
//!
//! A manifest names a slot; the owner names the folder. This module is the
//! sentence in between — the five rules a host path has to pass before podman
//! is handed a `-v` for it, and the refusal each one produces.
//!
//! **Checked twice, deliberately.** Once when the value is *stored*
//! (`agent_config_set`, a per-run override) and once when it is *used* (a phase
//! start, a service start), because the filesystem moves between the two: a
//! folder is renamed, a symlink is repointed, a disk is unmounted. Which of the
//! two is asking is [`Moment`], and the only thing it changes is what "the path
//! is not there" is called — `does not exist` while the owner is typing, and
//! `mount_path_missing` when a start would otherwise have run against nothing.
//!
//! **There is no allow-list.** Rule 4 names the places a mount may not point
//! and gives the reason in the refusal; everything it does not name is the
//! owner's choice, which is the whole point of the feature. Rule 5 is the one
//! rule that is about *other* agents: an `rw` mount is a tree a confined
//! container can rewrite, so nothing may resolve through it — which is why
//! **every** bound mount is indexed and not only the `rw` ones. A `ro` binding
//! that nests inside somebody's `rw` tree is the one the `rw` holder can
//! redirect.
//!
//! Pure, apart from the filesystem it has to ask and the one `async` gatherer
//! at the bottom: `$HOME`, the data directory and the runs root all arrive in
//! [`Ctx`] rather than being read here, so a test controls them and never
//! canonicalises against the real home directory. All three arrive
//! **canonical**, because rule 4 compares them against a canonical candidate:
//! a symlinked `/home`, a symlinked `LMGW_DATA_DIR` or a symlinked runtime dir
//! would otherwise leave the real directory behind them bindable.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::manifest::{Access, Manifest, MountField, MountKind};
use crate::state::SharedState;
use crate::store;

/// §5.3 rules 1–4: the path itself is wrong for a mount.
pub const REFUSED: &str = "mount_path_refused";
/// §5.3 rule 5: it overlaps a mount somebody else holds, and one of the two
/// is `rw`.
pub const NESTED: &str = "mount_path_nested";
/// A stored path that no longer exists when a start asks for it.
pub const MISSING: &str = "mount_path_missing";

/// Which side of the store/use pair is asking (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// `agent_config_set`, or a per-run override about to be used: a path that
    /// is not there is a `400` naming the field.
    Store,
    /// A phase or service start: a path that was there when it was saved and
    /// is not there now fails the start with [`MISSING`], and `podman` is never
    /// reached.
    Use,
}

/// A refused path, with the code the plane reports it under.
///
/// A `String` would have been enough for the message and would have left the
/// three codes of §5.9 to be re-derived by whoever rendered it; the code is
/// what the UI and a test name, so it travels with the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

/// One mount somebody already holds — rule 5's input.
///
/// Every bound mount, `ro` ones included: the rule is "no mount resolves
/// through a tree a container can rewrite", and a `ro` binding nested in
/// somebody's `rw` tree is exactly the one that holder can repoint under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub agent: String,
    pub field: String,
    /// Canonical as of the moment [`ctx`] read the catalog. A stored value
    /// that no longer resolves is not indexed at all — it cannot overlap
    /// anything real, and a name nothing answers for is not a tree.
    pub path: PathBuf,
    /// What the *manifest* declared for that slot. Rule 5 refuses an overlap
    /// when either side is [`Access::Rw`]; `ro` over `ro` is two readers of
    /// one tree and is allowed.
    pub access: Access,
}

/// What the rules are judged against, all of it passed in (§5.3).
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    /// `$HOME`, canonical. `None` is **not** "the home rules do not apply": it
    /// is "this gateway cannot tell where home is", and [`check`] refuses
    /// every mount rather than quietly dropping three of rule 4's bullets
    /// (principle 4).
    pub home: Option<PathBuf>,
    /// The lmgw data directory — `state.data_dir`, canonical.
    pub data_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/lmgw/<prefix>` or its fallback, as
    /// [`super::container::runs_root`] picks it — canonical.
    pub runs_root: PathBuf,
    /// Every mount bound across the whole catalog with its access, this slot's
    /// own included — [`check`] skips the slot it is checking by name.
    pub bound: Vec<Bound>,
}

/// Who is asking: the agent and field this value is for.
///
/// Rule 5 needs both — to skip the slot's own previous binding (re-saving the
/// same folder is not a conflict with itself) and to name the other agent and
/// field in the refusal.
#[derive(Debug, Clone, Copy)]
pub struct Slot<'a> {
    pub agent: &'a str,
    pub field: &'a str,
    pub kind: MountKind,
    /// This slot's declared access. Half of rule 5's question: an overlap is
    /// refused when either side is `rw`.
    pub access: Access,
}

/// One checked mount: the slot the manifest declared and the canonical host
/// path that fills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub field: MountField,
    pub host: PathBuf,
}

/// The five rules, in order, against one value (§5.3).
///
/// `Ok` is the **canonical** path: symlinks resolved, and that is what is
/// stored, mounted and printed — so what the owner picked and what podman
/// receives cannot differ by a link somebody moved.
pub fn check(slot: Slot<'_>, value: &str, ctx: &Ctx, moment: Moment) -> Result<PathBuf, Refusal> {
    let field = slot.field;
    let refused = |message: String| Refusal {
        code: REFUSED,
        message,
    };
    let raw = Path::new(value);

    // 1. Absolute, or refused. A relative path would be resolved against
    //    whatever directory the gateway happens to have been started in, which
    //    is not something the owner chose.
    if !raw.is_absolute() {
        return Err(refused(format!(
            "{field}: '{value}' is not an absolute path — a mount names an absolute path on the \
             gateway machine"
        )));
    }

    // 2. Canonicalised. The gap between this and podman's own resolution is
    //    §5.3's stated race; what it buys is that every rule below judges the
    //    path that will actually be mounted rather than a link to it.
    // Both halves end with the way out, because "the folder is gone" is the
    // one refusal an owner meets without having done anything: a save that
    // does not touch this slot goes through (§5.3 is checked per value), and
    // then the next start stops on it with no idea what is being asked of it.
    let missing = || match moment {
        Moment::Store => refused(format!(
            "{field}: {} does not exist — clear the field or point it at a folder that exists",
            raw.display()
        )),
        Moment::Use => Refusal {
            code: MISSING,
            message: format!(
                "{field}: {} no longer exists, so there is nothing to mount — clear the field or \
                 point it at a folder that exists",
                raw.display()
            ),
        },
    };
    let path = match std::fs::canonicalize(raw) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(e) => {
            return Err(refused(format!(
                "{field}: {} could not be resolved: {e}",
                raw.display()
            )))
        }
    };

    // 3. The kind matches the format the manifest declared.
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(e) => {
            return Err(refused(format!(
                "{field}: {} could not be read: {e}",
                path.display()
            )))
        }
    };
    let what = if meta.is_dir() {
        "a directory"
    } else if meta.is_file() {
        "a file"
    } else {
        "neither a directory nor a regular file"
    };
    match slot.kind {
        MountKind::Directory if !meta.is_dir() => {
            return Err(refused(format!(
                "{field}: a directory field names a directory, and {} is {what}",
                path.display()
            )))
        }
        MountKind::File if !meta.is_file() => {
            return Err(refused(format!(
                "{field}: a file field names a regular file, and {} is {what}",
                path.display()
            )))
        }
        _ => {}
    }

    // 4. Refused by location, with the reason in the message.
    //
    // Three of the bullets are about `$HOME`, so a gateway that cannot tell
    // where home is cannot judge them — and silently binding anyway would drop
    // exactly the rules that keep a mount off `.ssh` and `.gnupg`. Nothing
    // hidden (principle 4): it refuses, and says what to set.
    if ctx.home.is_none() {
        return Err(refused(format!(
            "{field}: the gateway cannot tell where the home directory is (set $HOME), so rule 4 \
             — which keeps a mount out of $HOME, $HOME/.ssh and $HOME/.gnupg — cannot be checked"
        )));
    }
    if let Some(reason) = location_reason(&path, ctx) {
        return Err(refused(format!(
            "{field}: {} cannot be bound — {reason}",
            path.display()
        )));
    }

    // 5. Not nested in another bound mount, in either direction, when either
    //    side is `rw`.
    //
    //    Indexing only the `rw` side was the hole: B binds `ro
    //    /srv/notes/daily`, A then binds `rw /srv/notes` — nothing overlaps an
    //    `rw` mount *at that moment*, because B's is `ro` — and A replaces
    //    `daily` with a symlink, so B's next use-time `canonicalize` lands
    //    wherever A pointed it. Either side being `rw` is what makes the pair
    //    a rewritable tree, and which of the two arrived first is not a
    //    security property.
    for other in &ctx.bound {
        if other.agent == slot.agent && other.field == field {
            continue;
        }
        // The *same* path bound twice is allowed, whatever the two access
        // modes are: that is what the shared `:z` label of §5.5 is for, and
        // two agents on one folder is an ordinary thing to want. There is no
        // tree to resolve *through* when both ends are the same node.
        if other.path == path {
            continue;
        }
        let how = if path.starts_with(&other.path) {
            "lies inside"
        } else if other.path.starts_with(&path) {
            "contains"
        } else {
            continue;
        };
        // `ro` over `ro` is two readers of one tree: neither container can
        // move anything the other resolves through, so there is nothing to
        // refuse.
        if slot.access == Access::Ro && other.access == Access::Ro {
            continue;
        }
        return Err(Refusal {
            code: NESTED,
            message: format!(
                "{field}: {} {how} {}, which agent '{}' has bound {} on '{}' — one of the two is \
                 rw, and a mount may not resolve through a tree another container can rewrite",
                path.display(),
                other.path.display(),
                other.agent,
                other.access.as_str(),
                other.field
            ),
        });
    }

    Ok(path)
}

/// Rule 4, alone: the reason this path is out of bounds, or `None`.
///
/// Order is the order §5.3 lists them in, and it matters — the data directory
/// normally sits *under* `$HOME`, so its "ancestors" clause would otherwise
/// answer for `$HOME` and `/` with the wrong sentence.
fn location_reason(path: &Path, ctx: &Ctx) -> Option<String> {
    let under = |ancestor: &Path| !ancestor.as_os_str().is_empty() && path.starts_with(ancestor);
    let over =
        |descendant: &Path| !descendant.as_os_str().is_empty() && descendant.starts_with(path);

    if path.parent().is_none() || ctx.home.as_deref().is_some_and(|h| over(h) && h != path) {
        return Some("too broad to relabel".to_string());
    }
    if let Some(home) = ctx.home.as_deref() {
        if path == home {
            return Some(
                "relabelling the home directory would break sshd, gpg and every other process \
                 that reads a label under it"
                    .to_string(),
            );
        }
        for name in [".ssh", ".gnupg"] {
            if path == home.join(name) {
                return Some(format!(
                    "relabelling {} would break sshd, gpg and every other process that reads a \
                     label under it",
                    path.display()
                ));
            }
        }
    }
    if under(&ctx.data_dir) || over(&ctx.data_dir) {
        return Some("the gateway's own database lives here".to_string());
    }
    if under(&ctx.runs_root) {
        return Some("run secrets live here".to_string());
    }
    for system in ["/proc", "/sys", "/dev", "/run", "/boot", "/etc"] {
        if path.starts_with(system) {
            return Some("not a data directory".to_string());
        }
    }
    None
}

/// Every mount value in `values`, checked against `manifest`'s slots.
///
/// The one call both moments make: `agent_config_set` before it writes, and a
/// start before it builds the argv. An absent or empty value is an **unbound**
/// slot and is skipped — whether that is allowed is the `required` rule's
/// business (`mount_unbound`, §5.9), not this module's.
pub fn check_values(
    agent_id: &str,
    manifest: &Manifest,
    values: &Map<String, Value>,
    ctx: &Ctx,
    moment: Moment,
) -> Result<Vec<Binding>, Refusal> {
    // Rule 5 against the call's *own* bindings as well as the catalog's. One
    // save carries two slots, and nothing had put the first one in the index
    // yet: `{ "a": "/srv/x", "b": "/srv/x/y" }` used to be written whole and
    // then refused against itself on every start and every save afterwards —
    // a row only a hand edit could get out of.
    let mut ctx = ctx.clone();
    let mut out = Vec::new();
    for field in manifest.mount_fields() {
        let value = values
            .get(&field.name)
            .and_then(Value::as_str)
            .unwrap_or("");
        if value.is_empty() {
            continue;
        }
        let slot = Slot {
            agent: agent_id,
            field: &field.name,
            kind: field.kind,
            access: field.access,
        };
        let host = check(slot, value, &ctx, moment)?;
        ctx.bound.push(Bound {
            agent: agent_id.to_string(),
            field: field.name.clone(),
            path: host.clone(),
            access: field.access,
        });
        out.push(Binding { field, host });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// What a reader that is not the owner is shown (§5.2, §5.6)
// ---------------------------------------------------------------------------

/// The slots this config actually **binds**, in the manifest's field order.
///
/// Nothing here touches the filesystem: "is this slot bound" is a question
/// about the config, and "is the folder still there" is [`check_values`]'. An
/// empty or absent value is an unbound slot, and an unbound slot is absent
/// everywhere — from the argv, from `input.json` and from the container's own
/// view of its row (§5.6).
pub fn bound_fields(manifest: &Manifest, values: &Map<String, Value>) -> Vec<MountField> {
    manifest
        .mount_fields()
        .filter(|f| {
            !values
                .get(&f.name)
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        })
        .collect()
}

/// Every mount field's value replaced by the container path, and every unbound
/// one dropped — principle 3, applied to a document (§5.2).
///
/// The one substitution, in one function, so the three readers that must not
/// see a host path cannot drift apart: `input.json`'s `config` (§5.6), the
/// `config` root a template resolves against (§5.6), and the container's own
/// read of its agent row (§3.10, which applies it to the finished DTO instead
/// because that document is built from the masked values).
pub fn as_container_paths(manifest: &Manifest, config: Value) -> Value {
    let Value::Object(mut map) = config else {
        return config;
    };
    for field in manifest.mount_fields() {
        let bound = !map
            .get(&field.name)
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty();
        match bound {
            true => {
                map.insert(field.name.clone(), Value::String(field.inside()));
            }
            // Absent rather than `""`: an optional slot nobody bound is a slot
            // the container has not been given, and an empty string in
            // `config.notes` would read as a path of no length.
            false => {
                map.remove(&field.name);
            }
        }
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// The gateway's half: what the rules are judged against on this box
// ---------------------------------------------------------------------------

/// `$HOME`, canonical, or the passwd entry behind this process.
///
/// An empty or relative `HOME` is not a home: matching `"."` against a
/// canonical path would only ever be wrong. But *dropping* it is not an option
/// either — rule 4's three home bullets would go with it — so the environment
/// is only the first place asked. `/etc/passwd` for the uid that owns
/// `/proc/self` is the second, which is where a service started without an
/// environment finds its own home. When neither answers, `None` travels into
/// [`Ctx`] and [`check`] refuses every mount rather than binding blind.
pub fn home() -> Option<PathBuf> {
    home_of(std::env::var("HOME").ok().as_deref(), passwd_home)
}

/// [`home`] with both sources handed in, so a test can ask what an unset or
/// relative `HOME` does without touching the process environment — which is
/// shared by every test in the binary.
fn home_of(env: Option<&str>, passwd: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    let home = env
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .or_else(passwd)?;
    // Canonical, like everything else rule 4 compares: `/home` is a symlink to
    // `/var/home` on Fedora Atomic, and a raw `$HOME` there would leave the
    // real home directory — and its `.ssh` — bindable by its other name.
    Some(canonical_or_raw(&home))
}

/// This process's home directory as `/etc/passwd` records it.
///
/// The uid comes from the owner of `/proc/self`, which is how
/// [`super::container::process_uid`] already answers the same question for the
/// `keep-id` note — no `libc`, and nothing that has to be kept in step with a
/// second way of asking.
fn passwd_home() -> Option<PathBuf> {
    let uid = super::container::process_uid()?;
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        // name:passwd:uid:gid:gecos:home:shell — fields 2 and 5. A short line
        // is skipped rather than ending the scan: a comment or an NIS `+`
        // entry must not hide the row three lines further down.
        let mut f = line.split(':').skip(2);
        let (Some(entry_uid), Some(home)) = (f.next(), f.nth(2)) else {
            continue;
        };
        if entry_uid.parse::<u32>().ok() == Some(uid) && Path::new(home).is_absolute() {
            return Some(PathBuf::from(home));
        }
    }
    None
}

/// Canonical when the path resolves, the value as given when it does not.
///
/// Rule 4's three boundaries are compared against a **canonical** candidate,
/// so they have to be canonical themselves or a symlink in front of one is a
/// way past it. The fallback is deliberate rather than an error: a runs root
/// that does not exist yet is the ordinary state of a gateway that has not run
/// anything, and refusing every mount over it would be the wrong trade.
fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// [`Ctx`] for this gateway, right now.
///
/// `bound` is read from **every** agent's stored config on every call rather
/// than cached: rule 5 is a statement about the catalog as it is at the moment
/// a path is bound or used, and a cache would answer for the catalog as it was.
/// A row whose manifest this build cannot read contributes nothing — it has no
/// readable schema, so it declares no mount fields anybody can name.
///
/// **Only what canonicalises now.** A stored value the importer never checked,
/// or one whose folder has since gone, is not indexed: `/` in somebody's
/// `config_values` would otherwise contain every path on the box and refuse
/// every other agent's mount, and a name nothing answers for is not a tree
/// anything can resolve through. The store-time rules on the import and
/// duplicate paths are the other half of that — this one keeps a row that got
/// in before them from poisoning the catalog.
///
/// The three boundaries are canonicalised here, once, for the reason rule 4
/// resolves the candidate: `/home` is a symlink on Fedora Atomic,
/// `LMGW_DATA_DIR` may be relative or a link, and `$XDG_RUNTIME_DIR` is a link
/// on more than one distribution. Comparing a canonical candidate against a
/// raw boundary is a way past all three.
pub async fn ctx(state: &SharedState) -> Ctx {
    let prefix = state.snapshot().settings.container_prefix.clone();
    let (runs_root, _) = super::container::runs_root(&state.data_dir, &prefix);
    let mut bound = Vec::new();
    for row in store::list_agents(&state.db).await.unwrap_or_default() {
        let Ok(agent) = super::Agent::from_row(row) else {
            continue;
        };
        let values = agent.config_values();
        for field in agent.manifest.mount_fields() {
            let value = values
                .get(&field.name)
                .and_then(Value::as_str)
                .unwrap_or("");
            if value.is_empty() {
                continue;
            }
            let Ok(path) = std::fs::canonicalize(value) else {
                continue;
            };
            bound.push(Bound {
                agent: agent.row.id.clone(),
                field: field.name,
                path,
                access: field.access,
            });
        }
    }
    Ctx {
        home: home(),
        data_dir: canonical_or_raw(&state.data_dir),
        runs_root: canonical_or_raw(&runs_root),
        bound,
    }
}

#[cfg(test)]
mod tests;
