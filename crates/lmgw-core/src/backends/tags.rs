//! Tags and the config hash (container-builds design §5 "Tags").
//!
//! - Moving: `<engine repo>:<slug>` — "follow this build".
//! - Immutable: `<engine repo>:<slug>-<base7>-<cfg6>` — "this exact image".
//!
//! `cfg6` is the head of a sha256 over every input that changes the image, so
//! the same inputs always give the same tag and different images never share
//! one. That is what makes the run's "already built" short-circuit (§5 step 2)
//! sound: if the immutable tag exists, the image it names *is* the image these
//! inputs would build.
//!
//! Everything here is pure. The run executor resolves the inputs (refs to
//! SHAs, "auto" to concrete values) and hands them over as a
//! [`ResolvedInputs`]; nothing in this module reads git, podman or the host.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::model::{BuildExtra, BuildRun, BuildSpec, Engine, GpuBackend, ResolvedInputs};
use super::validate;

pub use lmgw_api_types::builds::moving_tag;

/// The longest tag lmgw writes, whole reference included (§4): the OCI tag
/// limit, applied to `repo:tag` rather than to the tag alone so it holds for
/// any registry the image is ever pushed to.
pub const TAG_MAX: usize = 128;
/// Characters of the base commit SHA in an immutable tag.
pub const BASE_LEN: usize = 7;
/// Characters of the config hash in an immutable tag.
pub const CFG_LEN: usize = 6;

/// The longest engine image repository — the one a slug has to leave room
/// for, whatever engine the build is (or is later switched to).
const fn longest_repo() -> usize {
    let mut longest = 0;
    let mut i = 0;
    while i < Engine::ALL.len() {
        let len = Engine::ALL[i].image_repo().len();
        if len > longest {
            longest = len;
        }
        i += 1;
    }
    longest
}

/// The longest slug whose immutable tag still fits [`TAG_MAX`]:
/// `128 − len("localhost/lmgw-llama-server") − len(":") − len("-<base7>-<cfg6>")`
/// = 85. One bound for every engine, so switching a build's engine can never
/// make its slug too long.
pub const MAX_SLUG_LEN: usize = TAG_MAX - longest_repo() - 1 - (1 + BASE_LEN + 1 + CFG_LEN);

/// Version tag at the head of the canonical form. Bumped only if what is
/// hashed changes meaning — which re-tags every build once, deliberately.
const CFG_FORMAT: &str = "lmgw-build-cfg/1";

/// `<repo>:<slug>-<base7>-<cfg6>`. Refuses a SHA or hash too short to cut the
/// prefix from, rather than writing a shorter tag that could collide.
pub fn immutable_tag(
    engine: Engine,
    slug: &str,
    base_sha: &str,
    cfg_hash: &str,
) -> Result<String, String> {
    let base = hex_prefix("base commit SHA", base_sha, BASE_LEN)?;
    let cfg = hex_prefix("config hash", cfg_hash, CFG_LEN)?;
    Ok(format!("{}:{slug}-{base}-{cfg}", engine.image_repo()))
}

/// Where a dev instance tags (§10): `localhost/lmgw-dev-<engine repo>`, a
/// namespace of its own beside production's `localhost/lmgw-<engine repo>`,
/// so a dev build can never move — or be taken for — one of production's
/// tags. The two share the podman image store, not the names in it.
pub const DEV_REPO_PREFIX: &str = "localhost/lmgw-dev-";

/// The image repository builds of `engine` are tagged into on this instance:
/// [`Engine::image_repo`] in production, its [`DEV_REPO_PREFIX`] twin on a
/// dev instance.
pub fn instance_repo(engine: Engine, dev: bool) -> String {
    let repo = engine.image_repo();
    if dev {
        let name = repo.strip_prefix("localhost/lmgw-").unwrap_or(repo);
        format!("{DEV_REPO_PREFIX}{name}")
    } else {
        repo.to_string()
    }
}

/// [`moving_tag`] in this instance's namespace ([`instance_repo`]).
pub fn moving_tag_for(engine: Engine, slug: &str, dev: bool) -> String {
    format!("{}:{slug}", instance_repo(engine, dev))
}

/// [`immutable_tag`] in this instance's namespace ([`instance_repo`]). A dev
/// repository is 4 characters longer, so a slug at [`MAX_SLUG_LEN`] gives a
/// reference just over [`TAG_MAX`] there — the tag itself stays far inside
/// the OCI limit, and dev images are never pushed.
pub fn immutable_tag_for(
    engine: Engine,
    slug: &str,
    base_sha: &str,
    cfg_hash: &str,
    dev: bool,
) -> Result<String, String> {
    let base = hex_prefix("base commit SHA", base_sha, BASE_LEN)?;
    let cfg = hex_prefix("config hash", cfg_hash, CFG_LEN)?;
    Ok(format!(
        "{}:{slug}-{base}-{cfg}",
        instance_repo(engine, dev)
    ))
}

/// Whether `reference` is in the dev namespace ([`DEV_REPO_PREFIX`]) — the
/// only names a dev instance may tag, untag or remove.
pub fn is_dev_name(reference: &str) -> bool {
    reference.trim().starts_with(DEV_REPO_PREFIX)
}

/// The tag a run's image gets while it is not (yet) entitled to the immutable
/// tag: `<immutable>-r<run id>`. Every build is tagged this way first; a
/// verified image — or the first image of those inputs — then takes the
/// immutable tag too, and one that is neither (a broken or unverified
/// **Rebuild anyway** while the immutable tag names a verified image) keeps
/// only this, so a model pinned to the immutable tag keeps the good image.
pub fn run_tag(immutable: &str, run_id: i64) -> String {
    format!("{immutable}-r{run_id}")
}

fn hex_prefix<'a>(what: &str, value: &'a str, len: usize) -> Result<&'a str, String> {
    let head = value.get(..len).unwrap_or(value);
    if head.len() < len || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "the {what} '{value}' is not a hex string of at least {len} characters"
        ));
    }
    Ok(head)
}

/// The exact text [`cfg_hash`] hashes: a JSON array of `[name, value…]` pairs
/// in a fixed order. Arrays only — no objects, so there is no key order to
/// disagree about — and JSON string escaping makes it unambiguous whatever an
/// edit or a build arg contains. Public so a run log can show what was hashed.
///
/// What goes in, and why:
/// - **extras** — kind, identity and resolved SHA, in merge order. The pin
///   itself is not: a pin equal to the head builds the same image.
/// - **backend**, and the resolved **CUDA version**, **arch list**,
///   **Dockerfile path** and **target**.
/// - **edits** — find and replace. Not `required`: whether a miss fails the
///   run does not change the image a run that succeeds produces.
/// - **build args**, as parsed pairs, in order.
/// - **ccache** on/off, plus `CCACHE_MAXSIZE` when on (§5).
///
/// And what stays out: the repo URL and base ref (the base SHA is in the tag
/// already), `keep_layers`, `keep_runs` and `cpus` (they change how the build
/// runs and what is kept, never the image), the name and the notes.
pub fn canonical_cfg(spec: &BuildSpec, resolved: &ResolvedInputs) -> Result<String, String> {
    let extras: Vec<Value> = resolved
        .extras
        .iter()
        .map(|e| match &e.extra {
            BuildExtra::Pr { number, .. } => json!(["pr", number, e.sha]),
            BuildExtra::Ref {
                remote_url,
                git_ref,
                ..
            } => json!(["ref", remote_url, git_ref, e.sha]),
        })
        .collect();
    let edits: Vec<Value> = resolved
        .edits
        .iter()
        .map(|e| json!([e.find, e.replace]))
        .collect();
    let build_args: Vec<Value> = validate::parse_build_args(&spec.build_args)?
        .into_iter()
        .map(|(k, v)| json!([k, v]))
        .collect();
    let ccache = if spec.ccache {
        json!(["ccache", true, spec.ccache_max_size])
    } else {
        json!(["ccache", false])
    };
    let doc = json!([
        CFG_FORMAT,
        ["extras", extras],
        ["backend", spec.backend.as_str()],
        ["cuda_version", resolved.cuda_version],
        ["arch", resolved.arch],
        ["dockerfile", resolved.dockerfile],
        ["target", resolved.target],
        ["edits", edits],
        ["build_args", build_args],
        ccache,
    ]);
    Ok(doc.to_string())
}

/// Full lowercase sha256 hex of [`canonical_cfg`]. The immutable tag carries
/// its first [`CFG_LEN`] characters; the run row keeps all of it.
pub fn cfg_hash(spec: &BuildSpec, resolved: &ResolvedInputs) -> Result<String, String> {
    let canonical = canonical_cfg(spec, resolved)?;
    Ok(hex::encode(Sha256::digest(canonical.as_bytes())))
}

/// The ccache cache-mount id (§4): `lmgw-<engine>-<backend>` for a plain
/// build, `lmgw-<engine>-<backend>-<slug>` for one with extras — so PR code
/// never writes into the cache the master builds read. A build with extras
/// still *reads* the plain id, mounted read-only, to seed its own (§14.2,
/// [`presets::TemplateVars`](super::presets::TemplateVars)).
pub fn ccache_id(engine: Engine, backend: GpuBackend, slug: &str, has_extras: bool) -> String {
    if has_extras {
        format!("lmgw-{}-{}-{slug}", engine.as_str(), backend.as_str())
    } else {
        format!("lmgw-{}-{}", engine.as_str(), backend.as_str())
    }
}

/// The npm cache-mount id (§14.2 "An optional npm cache mount"), split the
/// same way as [`ccache_id`] and for the same reason: `lmgw-npm-<engine>` for
/// a plain build, `lmgw-npm-<engine>-<slug>` for one with extras. Per engine
/// rather than per `(engine, backend)` since the web UI build stage does not
/// depend on the backend.
pub fn npm_cache_id(engine: Engine, slug: &str, has_extras: bool) -> String {
    if has_extras {
        format!("lmgw-npm-{}-{slug}", engine.as_str())
    } else {
        format!("lmgw-npm-{}", engine.as_str())
    }
}

/// The per-build cache-mount ids a build's own runs may have written to: the
/// slug-suffixed forms of [`ccache_id`] and [`npm_cache_id`] (i.e. `has_extras
/// = true`), regardless of whether the build currently has extras. Unlike the
/// plain, non-suffixed ids, these can never name another build's cache or the
/// shared cache a plain build's run writes into — which is what makes it safe
/// for `build_set` delete to remove them outright.
pub fn own_cache_ids(engine: Engine, backend: GpuBackend, slug: &str) -> [String; 2] {
    [
        ccache_id(engine, backend, slug, true),
        npm_cache_id(engine, slug, true),
    ]
}

/// Every per-build cache id a build owns: [`own_cache_ids`] for its engine
/// and backend now **and** for each engine and backend one of its `runs` was
/// built with — a build switched from CUDA to Vulkan (or to another engine)
/// still owns the caches its earlier runs filled. In first-seen order, each
/// id once.
pub fn own_cache_ids_of(spec: &BuildSpec, runs: &[BuildRun]) -> Vec<String> {
    let combos = std::iter::once((spec.engine, spec.backend))
        .chain(runs.iter().map(|r| (r.engine, r.inputs.config.backend)));
    let mut out: Vec<String> = Vec::new();
    for (engine, backend) in combos {
        for id in own_cache_ids(engine, backend, &spec.slug) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// The on-disk directory buildah keeps a `--mount=type=cache,id=<id>` cache
/// mount's data in when the mount gives no explicit `uid`/`gid` (they default
/// to 0:0): the first 16 hex characters of `sha256("<id>:<uid>:<gid>")`
/// (§14.2 spike). Lives under `/var/tmp/buildah-cache-<host uid>/` — the host
/// uid, never the 0:0 the hash itself is computed with.
pub fn cache_mount_dir_name(id: &str) -> String {
    let digest = Sha256::digest(format!("{id}:0:0").as_bytes());
    hex::encode(digest)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::model::{BuildEdit, ResolvedExtra};

    const BASE: &str = "4b1a27fa0e4c1d2b3a4958677a8b9c0d1e2f3a4b";

    fn resolved() -> ResolvedInputs {
        ResolvedInputs {
            base_sha: BASE.into(),
            extras: vec![ResolvedExtra {
                extra: BuildExtra::Pr {
                    number: 16391,
                    pin: None,
                },
                sha: "1".repeat(40),
            }],
            cuda_version: Some("13.0.0".into()),
            arch: vec!["89".into()],
            dockerfile: ".devops/cuda.Dockerfile".into(),
            target: "server".into(),
            edits: vec![BuildEdit {
                find: "apt-get install -y".into(),
                replace: "apt-get install -y ccache".into(),
                required: true,
                ..BuildEdit::default()
            }],
        }
    }

    fn spec() -> BuildSpec {
        BuildSpec {
            slug: "official-master".into(),
            name: "official master".into(),
            repo_url: "https://github.com/ggml-org/llama.cpp".into(),
            git_ref: "master".into(),
            build_args: "GGML_CUDA_FA_ALL_QUANTS=ON".into(),
            ..BuildSpec::default()
        }
    }

    #[test]
    fn the_slug_bound_is_what_the_longest_repo_leaves_of_128() {
        assert_eq!("localhost/lmgw-llama-server".len(), 27);
        assert_eq!(MAX_SLUG_LEN, 128 - 27 - 1 - 15);
        assert_eq!(MAX_SLUG_LEN, 85);
        let longest = immutable_tag(
            Engine::Llama,
            &"s".repeat(MAX_SLUG_LEN),
            BASE,
            &"f".repeat(64),
        )
        .unwrap();
        assert_eq!(longest.len(), TAG_MAX);
    }

    #[test]
    fn a_dev_instance_tags_into_a_namespace_of_its_own() {
        assert_eq!(
            moving_tag_for(Engine::Llama, "official-master", true),
            "localhost/lmgw-dev-llama-server:official-master"
        );
        assert_eq!(
            moving_tag_for(Engine::Audio, "x", false),
            moving_tag(Engine::Audio, "x")
        );
        let dev = immutable_tag_for(Engine::Sdcpp, "master", BASE, "abcdef0123", true).unwrap();
        assert_eq!(dev, "localhost/lmgw-dev-sd-server:master-4b1a27f-abcdef");
        assert!(is_dev_name(&dev));
        assert!(!is_dev_name(
            &immutable_tag_for(Engine::Sdcpp, "master", BASE, "abcdef0123", false).unwrap()
        ));
        assert!(!is_dev_name("localhost/lmgw-llama-server:dev-x"));
        assert_eq!(
            run_tag("localhost/lmgw-sd-server:master-4b1a27f-abcdef", 12),
            "localhost/lmgw-sd-server:master-4b1a27f-abcdef-r12"
        );
    }

    #[test]
    fn a_builds_own_caches_include_every_backend_its_runs_used() {
        let mut spec = spec();
        spec.backend = GpuBackend::Vulkan;
        let run = |backend| {
            let mut config = spec.clone();
            config.backend = backend;
            BuildRun {
                engine: Engine::Llama,
                inputs: crate::backends::model::BuildRunInputs {
                    config,
                    ..Default::default()
                },
                ..Default::default()
            }
        };
        let ids = own_cache_ids_of(&spec, &[run(GpuBackend::Cuda), run(GpuBackend::Vulkan)]);
        assert_eq!(
            ids,
            vec![
                "lmgw-llama-vulkan-official-master".to_string(),
                "lmgw-npm-llama-official-master".to_string(),
                "lmgw-llama-cuda-official-master".to_string(),
            ]
        );
    }

    #[test]
    fn tags_are_the_repo_slug_base7_and_cfg6() {
        assert_eq!(
            moving_tag(Engine::Llama, "official-master"),
            "localhost/lmgw-llama-server:official-master"
        );
        assert_eq!(
            immutable_tag(Engine::Sdcpp, "master", BASE, "abcdef0123").unwrap(),
            "localhost/lmgw-sd-server:master-4b1a27f-abcdef"
        );
        assert!(immutable_tag(Engine::Audio, "x", "4b1a", "abcdef").is_err());
        assert!(immutable_tag(Engine::Audio, "x", BASE, "xyzxyz").is_err());
    }

    #[test]
    fn the_same_inputs_always_hash_the_same() {
        let a = cfg_hash(&spec(), &resolved()).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(a, cfg_hash(&spec(), &resolved()).unwrap());
        // Pinned in place: a change to the canonical form re-tags every build,
        // and must be a deliberate one (bump CFG_FORMAT), never a side effect.
        assert_eq!(
            canonical_cfg(&spec(), &resolved()).unwrap(),
            r#"["lmgw-build-cfg/1",["extras",[["pr",16391,"1111111111111111111111111111111111111111"]]],["backend","cuda"],["cuda_version","13.0.0"],["arch",["89"]],["dockerfile",".devops/cuda.Dockerfile"],["target","server"],["edits",[["apt-get install -y","apt-get install -y ccache"]]],["build_args",[["GGML_CUDA_FA_ALL_QUANTS","ON"]]],["ccache",true,"10G"]]"#
        );
    }

    #[test]
    fn every_image_affecting_input_moves_the_hash() {
        let base = cfg_hash(&spec(), &resolved()).unwrap();
        let moved = |s: BuildSpec, r: ResolvedInputs| cfg_hash(&s, &r).unwrap() != base;

        let mut r = resolved();
        r.extras[0].sha = "2".repeat(40);
        assert!(moved(spec(), r), "an extra's resolved SHA");
        let mut r = resolved();
        r.extras[0].extra = BuildExtra::Pr {
            number: 16392,
            pin: None,
        };
        assert!(moved(spec(), r), "which PR");
        let mut r = resolved();
        r.extras.push(ResolvedExtra {
            extra: BuildExtra::Ref {
                remote_url: "https://github.com/fork/llama.cpp".into(),
                git_ref: "x".into(),
                pin: None,
            },
            sha: "3".repeat(40),
        });
        assert!(moved(spec(), r), "an extra more");
        let mut r = resolved();
        r.extras.reverse();
        r.extras.push(ResolvedExtra {
            extra: BuildExtra::Pr {
                number: 1,
                pin: None,
            },
            sha: "4".repeat(40),
        });
        let mut r2 = resolved();
        r2.extras.insert(
            0,
            ResolvedExtra {
                extra: BuildExtra::Pr {
                    number: 1,
                    pin: None,
                },
                sha: "4".repeat(40),
            },
        );
        assert_ne!(
            cfg_hash(&spec(), &r).unwrap(),
            cfg_hash(&spec(), &r2).unwrap(),
            "merge order"
        );
        let mut s = spec();
        s.backend = GpuBackend::Vulkan;
        assert!(moved(s, resolved()), "backend");
        let mut r = resolved();
        r.cuda_version = Some("12.8.1".into());
        assert!(moved(spec(), r), "CUDA version");
        let mut r = resolved();
        r.arch = vec!["86".into(), "89".into()];
        assert!(moved(spec(), r), "arch list");
        let mut r = resolved();
        r.dockerfile = ".devops/llama-server-cuda.Dockerfile".into();
        assert!(moved(spec(), r), "Dockerfile");
        let mut r = resolved();
        r.target = "full".into();
        assert!(moved(spec(), r), "target");
        let mut r = resolved();
        r.edits[0].replace = "apt-get install -y zlib1g-dev".into();
        assert!(moved(spec(), r), "an edit");
        let mut r = resolved();
        r.edits.clear();
        assert!(moved(spec(), r), "no edits");
        let mut s = spec();
        s.build_args = "GGML_CUDA_FA_ALL_QUANTS=OFF".into();
        assert!(moved(s, resolved()), "build args");
        let mut s = spec();
        s.ccache = false;
        assert!(moved(s, resolved()), "ccache off");
        let mut s = spec();
        s.ccache_max_size = "20G".into();
        assert!(moved(s, resolved()), "ccache size");
    }

    #[test]
    fn what_does_not_change_the_image_does_not_move_the_hash() {
        let base = cfg_hash(&spec(), &resolved()).unwrap();
        let same = |s: BuildSpec, r: ResolvedInputs| cfg_hash(&s, &r).unwrap() == base;

        let mut s = spec();
        s.keep_layers = true;
        s.keep_runs = Some(3);
        s.cpus = Some("0-15".into());
        s.name = "renamed".into();
        s.notes = "a note".into();
        s.slug = "other-slug".into();
        assert!(same(s, resolved()), "run-shaping fields");
        let mut r = resolved();
        r.edits[0].required = false;
        assert!(same(spec(), r), "an edit's required flag");
        let mut r = resolved();
        r.extras[0].extra = BuildExtra::Pr {
            number: 16391,
            pin: Some("1".repeat(40)),
        };
        assert!(same(spec(), r), "a pin equal to the head");
        let mut s = spec();
        s.build_args = "\n  GGML_CUDA_FA_ALL_QUANTS=ON  \n\n".into();
        assert!(same(s, resolved()), "build-args whitespace");
        // With ccache off its size is not an input.
        let mut off = spec();
        off.ccache = false;
        let off_hash = cfg_hash(&off, &resolved()).unwrap();
        off.ccache_max_size = "1G".into();
        assert_eq!(cfg_hash(&off, &resolved()).unwrap(), off_hash);
    }

    #[test]
    fn a_build_arg_that_does_not_parse_is_refused_not_hashed() {
        let mut s = spec();
        s.build_args = "NOT A PAIR".into();
        assert!(cfg_hash(&s, &resolved()).is_err());
    }

    #[test]
    fn pr_builds_get_their_own_ccache() {
        assert_eq!(
            ccache_id(Engine::Llama, GpuBackend::Cuda, "official-master", false),
            "lmgw-llama-cuda"
        );
        assert_eq!(
            ccache_id(Engine::Llama, GpuBackend::Cuda, "master-pr1", true),
            "lmgw-llama-cuda-master-pr1"
        );
    }

    #[test]
    fn npm_ids_split_the_same_way_as_ccache() {
        assert_eq!(
            npm_cache_id(Engine::Llama, "official-master", false),
            "lmgw-npm-llama"
        );
        assert_eq!(
            npm_cache_id(Engine::Llama, "master-pr1", true),
            "lmgw-npm-llama-master-pr1"
        );
    }

    #[test]
    fn own_cache_ids_are_always_the_slug_suffixed_form() {
        // Regardless of whether the build has extras right now: these two ids
        // can only ever be this build's own, never the shared cache.
        assert_eq!(
            own_cache_ids(Engine::Llama, GpuBackend::Cuda, "master-pr1"),
            [
                "lmgw-llama-cuda-master-pr1".to_string(),
                "lmgw-npm-llama-master-pr1".to_string(),
            ]
        );
    }

    #[test]
    fn the_cache_mount_dir_name_matches_the_spike() {
        // Spike finding (§14.2): id `lmgw-llama-cuda` (a plain build's ccache
        // mount, uid/gid defaulting to 0:0) hashes to this directory name.
        assert_eq!(cache_mount_dir_name("lmgw-llama-cuda"), "3b85a8397e0a7074");
    }
}
