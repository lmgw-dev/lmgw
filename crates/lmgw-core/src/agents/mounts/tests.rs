//! §10, part 3: one test per bullet of the §5.3 rule-4 list, symlink
//! canonicalisation, a path that vanishes between store and use, rule 5 in
//! both directions and both access pairings, its same-path exception, the two
//! kind mismatches, and a relative path.
//!
//! Plus the three the review asked for: the boundaries themselves reached
//! through a symlink, a gateway that cannot tell where `$HOME` is, and two
//! nested slots arriving in **one** [`check_values`] call, which nothing had
//! indexed yet when the second one was judged.
//!
//! Every path here is under a tempdir and every boundary — `$HOME`, the data
//! directory, the runs root — is handed over in [`Ctx`]: the rules are about
//! places that exist on the machine running the tests, and a suite that read
//! the real `$HOME` would be one `canonicalize` away from asserting against
//! somebody's actual `.ssh`.

use super::*;

/// A tempdir standing in for a home directory, with the two named children and
/// somewhere ordinary to bind.
struct Box0 {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    runs_root: PathBuf,
    notes: PathBuf,
}

fn boxed() -> Box0 {
    let tmp = tempfile::tempdir().unwrap();
    // Canonical from the start: macOS and some Linux setups hand out a
    // symlinked `/tmp`, and every path `check` returns has been through
    // `canonicalize`.
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("home/alice");
    let data_dir = home.join(".local/share/lmgw");
    let runs_root = root.join("run/user/1000/lmgw/lmgw");
    let notes = home.join("Notes");
    for d in [
        &home.join(".ssh"),
        &home.join(".gnupg"),
        &data_dir,
        &runs_root,
        &notes,
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    Box0 {
        _tmp: tmp,
        home,
        data_dir,
        runs_root,
        notes,
    }
}

impl Box0 {
    fn ctx(&self) -> Ctx {
        Ctx {
            home: Some(self.home.clone()),
            data_dir: self.data_dir.clone(),
            runs_root: self.runs_root.clone(),
            bound: Vec::new(),
        }
    }
}

fn slot(kind: MountKind) -> Slot<'static> {
    Slot {
        agent: "notes-desk",
        field: "notes",
        kind,
        access: Access::Rw,
    }
}

fn ro_slot(kind: MountKind) -> Slot<'static> {
    Slot {
        access: Access::Ro,
        ..slot(kind)
    }
}

/// One binding held by somebody else, for rule 5.
fn held(path: &Path, access: Access) -> Bound {
    Bound {
        agent: "archivist".to_string(),
        field: "root".to_string(),
        path: path.to_path_buf(),
        access,
    }
}

fn dir_check(value: &Path, ctx: &Ctx) -> Result<PathBuf, Refusal> {
    check(
        slot(MountKind::Directory),
        &value.display().to_string(),
        ctx,
        Moment::Store,
    )
}

#[test]
fn an_ordinary_folder_is_bound_and_comes_back_canonical() {
    let b = boxed();
    assert_eq!(dir_check(&b.notes, &b.ctx()).unwrap(), b.notes);
}

#[test]
fn a_relative_path_is_refused_naming_the_field() {
    let b = boxed();
    let e = check(slot(MountKind::Directory), "Notes", &b.ctx(), Moment::Store).unwrap_err();
    assert_eq!(e.code, REFUSED);
    assert!(e.message.starts_with("notes: "), "{}", e.message);
    assert!(e.message.contains("absolute path"), "{}", e.message);
}

/// §5.3 rule 4, bullet by bullet: the reason is in the message, and the field
/// is in front of it.
#[test]
fn every_refused_location_says_which_rule_refused_it() {
    let b = boxed();
    let ctx = b.ctx();
    let root = b.home.parent().unwrap().parent().unwrap().to_path_buf();
    for (path, needle) in [
        // `/` and every ancestor of `$HOME`.
        (PathBuf::from("/"), "too broad to relabel"),
        (root.clone(), "too broad to relabel"),
        (
            b.home.parent().unwrap().to_path_buf(),
            "too broad to relabel",
        ),
        // `$HOME` itself.
        (
            b.home.clone(),
            "relabelling the home directory would break sshd, gpg",
        ),
        // The two by name.
        (b.home.join(".ssh"), "would break sshd, gpg"),
        (b.home.join(".gnupg"), "would break sshd, gpg"),
        // The data directory, under it, and its ancestors.
        (b.data_dir.clone(), "the gateway's own database lives here"),
        (
            b.data_dir.join("agents"),
            "the gateway's own database lives here",
        ),
        (
            b.home.join(".local/share"),
            "the gateway's own database lives here",
        ),
        // The runs root and under it.
        (b.runs_root.clone(), "run secrets live here"),
        (b.runs_root.join("run-1"), "run secrets live here"),
        // The system directories.
        (PathBuf::from("/proc"), "not a data directory"),
        (PathBuf::from("/sys"), "not a data directory"),
        (PathBuf::from("/dev"), "not a data directory"),
        (PathBuf::from("/boot"), "not a data directory"),
        (PathBuf::from("/etc"), "not a data directory"),
        (PathBuf::from("/etc/ssl"), "not a data directory"),
    ] {
        if !path.exists() {
            // `/boot` is absent in a container and `/etc/ssl` on a minimal
            // image; the rule under test is about the location, and rule 2 is
            // asserted on its own below.
            continue;
        }
        std::fs::create_dir_all(&path).ok();
        let e = dir_check(&path, &ctx).unwrap_err();
        assert_eq!(e.code, REFUSED, "{}: {}", path.display(), e.message);
        assert!(
            e.message.contains(needle),
            "{} refused without its reason: {}",
            path.display(),
            e.message
        );
        assert!(
            e.message.starts_with("notes: "),
            "{} refused without the field: {}",
            path.display(),
            e.message
        );
    }
    // `/run` is the runs root's usual home and is on the system list too; the
    // reason it gives is whichever matched first, so it is asserted as refused
    // rather than for one sentence.
    assert!(dir_check(Path::new("/run"), &ctx).is_err());
}

#[test]
fn a_symlink_stores_the_directory_it_points_at() {
    let b = boxed();
    let link = b.home.join("notes-link");
    std::os::unix::fs::symlink(&b.notes, &link).unwrap();
    assert_eq!(dir_check(&link, &b.ctx()).unwrap(), b.notes);
}

/// A symlink into a refused place is refused by where it *lands*, which is the
/// whole reason rule 2 runs before rule 4.
#[test]
fn a_symlink_into_a_refused_place_is_refused_by_its_target() {
    let b = boxed();
    let link = b.home.join("innocent");
    std::os::unix::fs::symlink(b.home.join(".ssh"), &link).unwrap();
    let e = dir_check(&link, &b.ctx()).unwrap_err();
    assert!(e.message.contains("would break sshd, gpg"), "{}", e.message);
    assert!(e.message.contains(".ssh"), "{}", e.message);
}

#[test]
fn a_path_that_vanishes_between_store_and_use_is_named_by_each_moment() {
    let b = boxed();
    let gone = b.home.join("Archive");
    std::fs::create_dir_all(&gone).unwrap();
    assert_eq!(dir_check(&gone, &b.ctx()).unwrap(), gone);

    std::fs::remove_dir(&gone).unwrap();
    let value = gone.display().to_string();
    let at_store = check(slot(MountKind::Directory), &value, &b.ctx(), Moment::Store).unwrap_err();
    assert_eq!(at_store.code, REFUSED);
    assert!(at_store.message.contains("does not exist"), "{at_store:?}");

    let at_use = check(slot(MountKind::Directory), &value, &b.ctx(), Moment::Use).unwrap_err();
    assert_eq!(at_use.code, MISSING);
    assert!(at_use.message.starts_with("notes: "), "{at_use:?}");
    assert!(at_use.message.contains(&value), "{at_use:?}");
}

#[test]
fn a_file_for_a_directory_field_and_a_directory_for_a_file_field_are_both_refused() {
    let b = boxed();
    let ctx = b.ctx();
    let file = b.home.join("key.pem");
    std::fs::write(&file, "x").unwrap();

    let e = dir_check(&file, &ctx).unwrap_err();
    assert_eq!(e.code, REFUSED);
    assert!(
        e.message.contains("a directory field names a directory"),
        "{}",
        e.message
    );

    let e = check(
        slot(MountKind::File),
        &b.notes.display().to_string(),
        &ctx,
        Moment::Store,
    )
    .unwrap_err();
    assert!(
        e.message.contains("a file field names a regular file"),
        "{}",
        e.message
    );

    // And the pair that fits.
    assert_eq!(
        check(
            slot(MountKind::File),
            &file.display().to_string(),
            &ctx,
            Moment::Store
        )
        .unwrap(),
        file
    );
}

/// §5.3 rule 5, both directions, and the exception that makes the shared label
/// of §5.5 worth having.
#[test]
fn a_path_may_not_nest_with_another_agents_rw_mount_in_either_direction() {
    let b = boxed();
    let inner = b.notes.join("daily");
    std::fs::create_dir_all(&inner).unwrap();

    // Somebody else holds the parent, rw.
    let mut ctx = b.ctx();
    ctx.bound = vec![held(&b.notes, Access::Rw)];
    let e = dir_check(&inner, &ctx).unwrap_err();
    assert_eq!(e.code, NESTED);
    assert!(e.message.contains("lies inside"), "{}", e.message);
    assert!(e.message.contains("'archivist'"), "{}", e.message);
    assert!(e.message.contains("'root'"), "{}", e.message);
    assert!(e.message.contains(" rw on "), "{}", e.message);

    // The other way round: somebody else holds the child, rw.
    ctx.bound = vec![Bound {
        field: "daily".to_string(),
        ..held(&inner, Access::Rw)
    }];
    let e = dir_check(&b.notes, &ctx).unwrap_err();
    assert_eq!(e.code, NESTED);
    assert!(e.message.contains("contains"), "{}", e.message);
    assert!(e.message.contains("'daily'"), "{}", e.message);

    // The same path twice is allowed — two agents on one folder is the case
    // `:z` exists for — whichever access modes the two declare.
    for access in [Access::Ro, Access::Rw] {
        ctx.bound = vec![held(&b.notes, access)];
        assert_eq!(dir_check(&b.notes, &ctx).unwrap(), b.notes);
    }

    // And a slot does not collide with its own previous binding.
    ctx.bound = vec![Bound {
        agent: "notes-desk".to_string(),
        field: "notes".to_string(),
        path: b.notes.clone(),
        access: Access::Rw,
    }];
    assert_eq!(dir_check(&inner, &ctx).unwrap(), inner);
}

/// Rule 5 indexes **every** bound mount, not only the `rw` ones, and refuses an
/// overlap when either side is `rw` — in both arrival orders, because which of
/// the two was saved first is not a security property.
///
/// The hole this closes: B binds `ro /srv/notes/daily`, A then binds `rw
/// /srv/notes` — nothing overlapped an `rw` mount at that moment — and A
/// replaces `daily` with a symlink, so B's next use-time `canonicalize` lands
/// wherever A pointed it.
#[test]
fn a_ro_mount_and_an_rw_mount_may_not_nest_in_either_arrival_order() {
    let b = boxed();
    let inner = b.notes.join("daily");
    std::fs::create_dir_all(&inner).unwrap();
    let mut ctx = b.ctx();

    // B holds the child `ro`; A now asks for the parent `rw`.
    ctx.bound = vec![Bound {
        field: "daily".to_string(),
        ..held(&inner, Access::Ro)
    }];
    let e = dir_check(&b.notes, &ctx).unwrap_err();
    assert_eq!(e.code, NESTED, "{}", e.message);
    assert!(e.message.contains(" ro on 'daily'"), "{}", e.message);
    assert!(e.message.contains("one of the two is rw"), "{}", e.message);

    // The other order: A holds the parent `rw`, B now asks for the child `ro`.
    ctx.bound = vec![held(&b.notes, Access::Rw)];
    let e = check(
        ro_slot(MountKind::Directory),
        &inner.display().to_string(),
        &ctx,
        Moment::Store,
    )
    .unwrap_err();
    assert_eq!(e.code, NESTED, "{}", e.message);
    assert!(e.message.contains(" rw on 'root'"), "{}", e.message);

    // `ro` over `ro` is two readers of one tree: neither can move anything the
    // other resolves through, so it is allowed.
    ctx.bound = vec![Bound {
        field: "daily".to_string(),
        ..held(&inner, Access::Ro)
    }];
    assert_eq!(
        check(
            ro_slot(MountKind::Directory),
            &b.notes.display().to_string(),
            &ctx,
            Moment::Store
        )
        .unwrap(),
        b.notes
    );
}

/// A gateway that cannot tell where home is refuses **every** mount, rather
/// than dropping the three rule-4 bullets that are about `$HOME` and binding
/// the rest (principle 4).
///
/// It used to bind: `home: None` simply meant the home clauses matched
/// nothing, so on a box with no `$HOME` and no passwd entry for its uid,
/// `.ssh` was an ordinary folder.
#[test]
fn a_gateway_that_cannot_find_home_refuses_every_mount() {
    let b = boxed();
    let ctx = Ctx {
        home: None,
        ..b.ctx()
    };
    let e = dir_check(&b.notes, &ctx).unwrap_err();
    assert_eq!(e.code, REFUSED);
    assert!(e.message.starts_with("notes: "), "{}", e.message);
    assert!(
        e.message
            .contains("cannot tell where the home directory is (set $HOME)"),
        "{}",
        e.message
    );
    assert!(e.message.contains("rule 4"), "{}", e.message);
    // Even the folder that would have been fine, and the ones that were never
    // about home either way.
    assert!(dir_check(&b.home.join(".ssh"), &ctx).is_err());
    assert!(dir_check(&b.data_dir, &ctx).is_err());
    // The rules in front of it still answer first: "the gateway has no home"
    // is not the sentence for a typo.
    let e = check(slot(MountKind::Directory), "Notes", &ctx, Moment::Store).unwrap_err();
    assert!(e.message.contains("absolute path"), "{}", e.message);
}

/// [`home`]'s two sources, without touching the process environment — which is
/// shared by every test in this binary.
#[test]
fn home_falls_back_to_passwd_and_then_to_nothing() {
    let b = boxed();
    let passwd = || Some(b.home.clone());
    // Unset, and empty or relative, are all "the environment did not say".
    for env in [None, Some(""), Some("."), Some("home/alice")] {
        assert_eq!(home_of(env, passwd), Some(b.home.clone()), "{env:?}");
    }
    // An absolute `$HOME` wins, and comes back canonical.
    let link = b.home.parent().unwrap().join("alice-link");
    std::os::unix::fs::symlink(&b.home, &link).unwrap();
    assert_eq!(
        home_of(Some(&link.display().to_string()), passwd),
        Some(b.home.clone())
    );
    // Neither source answers: `None`, which is what makes `check` refuse.
    assert_eq!(home_of(None, || None), None);
}

/// §5.3 rule 4 compares a **canonical** candidate, so the three boundaries it
/// compares against have to be canonical too.
///
/// On Fedora Atomic `/home` is a symlink to `/var/home`; a relative or
/// symlinked `LMGW_DATA_DIR` is an ordinary thing to have. Against a raw
/// boundary, the real directory behind the link is simply another path — and
/// the data directory is the one holding the database with every key in it.
#[test]
fn a_symlinked_home_and_data_directory_are_still_refused_by_their_real_path() {
    let b = boxed();
    let root = b.home.parent().unwrap().parent().unwrap().to_path_buf();
    // `<root>/houses -> <root>/home`, the shape `/home -> /var/home` has.
    let houses = root.join("houses");
    std::os::unix::fs::symlink(root.join("home"), &houses).unwrap();
    // `<root>/data -> <home>/.local/share/lmgw`, an `LMGW_DATA_DIR` on a disk
    // reached through a link.
    let data_link = root.join("data");
    std::os::unix::fs::symlink(&b.data_dir, &data_link).unwrap();
    // The runs root through a link of its own.
    let runs_link = root.join("runs");
    std::os::unix::fs::symlink(&b.runs_root, &runs_link).unwrap();

    // What `ctx` hands over: canonical, whatever it was given.
    let ctx = Ctx {
        home: home_of(Some(&houses.join("alice").display().to_string()), || None),
        data_dir: canonical_or_raw(&data_link),
        runs_root: canonical_or_raw(&runs_link),
        bound: Vec::new(),
    };
    assert_eq!(ctx.home.as_deref(), Some(b.home.as_path()));

    // And every rule-4 bullet answers for the real path as well as the link.
    for (path, needle) in [
        (b.home.join(".ssh"), "would break sshd, gpg"),
        (houses.join("alice/.ssh"), "would break sshd, gpg"),
        (b.data_dir.clone(), "the gateway's own database lives here"),
        (data_link.clone(), "the gateway's own database lives here"),
        (b.runs_root.clone(), "run secrets live here"),
        (runs_link.clone(), "run secrets live here"),
    ] {
        let e = dir_check(&path, &ctx).unwrap_err();
        assert_eq!(e.code, REFUSED, "{}: {}", path.display(), e.message);
        assert!(
            e.message.contains(needle),
            "{} refused for the wrong reason: {}",
            path.display(),
            e.message
        );
    }

    // The same boundaries left raw are the bug this is about: the real data
    // directory reads as an ordinary folder.
    let raw = Ctx {
        home: Some(houses.join("alice")),
        data_dir: data_link,
        runs_root: runs_link,
        bound: Vec::new(),
    };
    assert_eq!(dir_check(&b.data_dir, &raw).unwrap(), b.data_dir);
}

// ---------------------------------------------------------------------------
// `check_values`: the whole call, and the bindings it makes on the way
// ---------------------------------------------------------------------------

/// A manifest with two `rw` directory slots, `a` and `b`.
fn two_slots() -> Manifest {
    crate::agents::manifest::load(
        r#"{ "schema_version": 1, "id": "notes-desk", "name": "Notes desk",
              "model": { "alias": "m1" },
              "config": { "schema": { "type": "object", "properties": {
                "a": { "type": "string", "format": "directory", "access": "rw" },
                "b": { "type": "string", "format": "directory", "access": "rw" }
              } } },
              "run": { "kind": "container", "image": "ghcr.io/acme/notes:1" } }"#,
    )
    .expect("the fixture manifest parses")
}

/// Rule 5 against the call's **own** bindings, not only the catalog's.
///
/// One save carries both slots and nothing had indexed the first one yet, so
/// `{ "a": "/srv/x", "b": "/srv/x/y" }` was written whole — and then refused
/// against itself on every start and every save afterwards, a row only a hand
/// edit could get out of.
#[test]
fn two_nested_slots_in_one_call_are_refused_against_each_other() {
    let b = boxed();
    let inner = b.notes.join("daily");
    std::fs::create_dir_all(&inner).unwrap();
    let m = two_slots();
    let values = |first: &Path, second: &Path| {
        let mut map = Map::new();
        map.insert("a".to_string(), Value::String(first.display().to_string()));
        map.insert("b".to_string(), Value::String(second.display().to_string()));
        map
    };

    for (first, second) in [(&b.notes, &inner), (&inner, &b.notes)] {
        let e = check_values(
            "notes-desk",
            &m,
            &values(first, second),
            &b.ctx(),
            Moment::Store,
        )
        .unwrap_err();
        assert_eq!(e.code, NESTED, "{}", e.message);
    }

    // Two slots on the same folder are still fine, and so are two that do not
    // meet.
    let sideways = b.home.join("Archive");
    std::fs::create_dir_all(&sideways).unwrap();
    for (first, second) in [(&b.notes, &b.notes), (&b.notes, &sideways)] {
        let bound = check_values(
            "notes-desk",
            &m,
            &values(first, second),
            &b.ctx(),
            Moment::Store,
        )
        .expect("two slots that do not nest");
        assert_eq!(bound.len(), 2);
    }
}
