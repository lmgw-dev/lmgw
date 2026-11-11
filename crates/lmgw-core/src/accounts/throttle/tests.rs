use super::*;

const A: &str = "192.0.2.1";

fn from(name: &str, address: &str) -> Attempt {
    Attempt::sign_in(name, vec![address.to_string()], None)
}

fn known(name: &str, address: &str, token_hash: &str) -> Attempt {
    Attempt::sign_in(name, vec![address.to_string()], Some(token_hash))
}

/// One failed attempt at `now`: reserved and left counted.
fn fail(t: &Throttle, a: &Attempt, now: Instant) {
    let r = t.reserve_at(a, now).expect("not held");
    drop(r);
}

/// `n` counted failures at `now`, past the check: what `n` failed attempts
/// spread over time leave behind.
fn failed_n(t: &Throttle, a: &Attempt, now: Instant, n: u32) {
    let mut entries = t.entries.lock().unwrap();
    for k in a.keys() {
        let e = entries.entry(k).or_insert(Entry {
            failures: 0,
            last: now,
            noted: false,
        });
        e.failures += n;
        e.last = now;
    }
}

#[test]
fn the_delay_doubles_to_its_cap_and_a_success_resets_it() {
    let t = Throttle::default();
    let now = Instant::now();
    let owner = from("owner", A);
    fail(&t, &owner, now);
    assert_eq!(
        t.reserve_at(&owner, now).unwrap_err(),
        (Duration::from_secs(1), Held::Name)
    );
    failed_n(&t, &owner, now, 2);
    assert_eq!(
        t.reserve_at(&owner, now).unwrap_err().0,
        Duration::from_secs(4)
    );
    failed_n(&t, &owner, now, 30);
    assert_eq!(t.reserve_at(&owner, now).unwrap_err().0, LOGIN_BACKOFF_MAX);
    // Another address trying the name is held by the name's delay; the
    // address key holds every name from the failing address.
    assert!(t.reserve_at(&from("owner", "192.0.2.9"), now).is_err());
    assert_eq!(
        t.reserve_at(&from("sam", A), now).unwrap_err().1,
        Held::Address
    );
    // A good sign-in from elsewhere, once the delays ran out, clears the
    // name.
    let later = now + LOGIN_BACKOFF_MAX;
    let r = t.reserve_at(&from("owner", "198.51.100.1"), later).unwrap();
    t.succeeded(r);
    assert!(t.reserve_at(&from("owner", "192.0.2.9"), later).is_ok());
}

#[test]
fn a_reservation_holds_a_concurrent_attempt_and_a_success_takes_it_back() {
    let t = Throttle::default();
    let now = Instant::now();
    let first = t.reserve_at(&from("owner", A), now).unwrap();
    // The second attempt inside the first one's window is refused, whatever
    // the name, before the first has been verified.
    assert_eq!(
        t.reserve_at(&from("owner", A), now).unwrap_err().1,
        Held::Name
    );
    assert_eq!(
        t.reserve_at(&from("sam", A), now).unwrap_err().1,
        Held::Address
    );
    t.succeeded(first);
    assert!(t.is_empty(), "a good sign-in leaves nothing behind");
    // An undone attempt (no permit was free) leaves nothing either, and an
    // earlier failure keeps its own time.
    fail(&t, &from("sam", A), now);
    let later = now + Duration::from_secs(2);
    let r = t.reserve_at(&from("kim", A), later).unwrap();
    t.undo(r);
    assert_eq!(t.len(), 3, "sam's three keys only");
    let entries = t.entries.lock().unwrap();
    let address = &entries[&Key::Address(A.into())];
    assert_eq!((address.failures, address.last), (1, now));
}

#[test]
fn concurrent_attempts_inside_an_open_window_are_refused() {
    let t = std::sync::Arc::new(Throttle::default());
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let t = t.clone();
            std::thread::spawn(move || t.reserve(&from("owner", A)).ok())
        })
        .collect();
    let passed: Vec<_> = threads
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(
        passed.len(),
        1,
        "one attempt goes, the others meet its delay"
    );
}

#[test]
fn a_known_browser_is_held_by_its_own_pair_only() {
    let t = Throttle::default();
    let now = Instant::now();
    // A spray from many addresses against one name, and one from the known
    // browser's own address.
    for i in 0..10 {
        failed_n(&t, &from("owner", &format!("198.51.100.{i}")), now, 1);
    }
    failed_n(&t, &from("owner", "203.0.113.5"), now, 5);
    assert!(t.reserve_at(&from("owner", "203.0.113.5"), now).is_err());
    let mine = known("owner", "203.0.113.5", "aaaa");
    let r = t
        .reserve_at(&mine, now)
        .expect("the spray does not hold it");
    drop(r);
    // Its own pair does.
    assert!(t.reserve_at(&mine, now).is_err());
    // Another known browser of the account behind the same address is not
    // held by the first one's mistakes.
    assert!(t
        .reserve_at(&known("owner", "203.0.113.5", "bbbb"), now)
        .is_ok());
}

#[test]
fn a_capped_entry_is_kept_past_its_delay_so_the_next_failure_stays_capped() {
    let t = Throttle::default();
    let then = Instant::now();
    let owner = from("owner", A);
    failed_n(&t, &owner, then, 30);
    // The delay ran out, and the cap has passed once more since: still there.
    let after_one = then + LOGIN_BACKOFF_MAX + LOGIN_BACKOFF_MAX - Duration::from_secs(1);
    let r = t.reserve_at(&owner, after_one).unwrap();
    drop(r);
    assert_eq!(
        t.reserve_at(&owner, after_one).unwrap_err().0,
        LOGIN_BACKOFF_MAX
    );
    // Gone once its delay and the cap have both passed since its last.
    let gone = after_one + LOGIN_BACKOFF_MAX + LOGIN_BACKOFF_MAX;
    let r = t.reserve_at(&from("other", "192.0.2.7"), gone).unwrap();
    drop(r);
    assert_eq!(t.len(), 3, "the old ones are pruned on the next attempt");
}

#[test]
fn names_are_keyed_by_their_hash_and_unknown_names_count_alike() {
    let t = Throttle::default();
    let now = Instant::now();
    fail(&t, &from("No-Such-Name ", A), now);
    assert!(t
        .reserve_at(&from("no-such-name", "192.0.2.50"), now)
        .is_err());
    let entries = t.entries.lock().unwrap();
    assert!(entries
        .keys()
        .all(|k| !format!("{k:?}").contains("no-such")));
    assert_eq!(name_key(" Owner"), name_key("owner"));
    assert_eq!(name_key("owner").len(), 64);
}

#[test]
fn ipv6_counts_by_its_64_and_its_56() {
    let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
    let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
    let c: IpAddr = "2001:db8:1:2f:cccc::9".parse().unwrap();
    assert_eq!(address_keys(a), address_keys(b));
    assert_eq!(address_keys(a), ["2001:db8:1:2::/64", "2001:db8:1:0::/56"]);
    assert_ne!(address_keys(a)[0], address_keys(c)[0]);
    assert_eq!(address_keys(a)[1], address_keys(c)[1]);
    let mapped: IpAddr = "::ffff:192.0.2.4".parse().unwrap();
    assert_eq!(address_keys(mapped), ["192.0.2.4"]);
    assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
    assert_eq!(throttled(Duration::from_secs(32), Held::Name).status, 429);
    // A host walking its /64s inside one /56 is held by the /56.
    let t = Throttle::default();
    let now = Instant::now();
    failed_n(
        &t,
        &Attempt::sign_in("owner", address_keys(a), None),
        now,
        1,
    );
    assert_eq!(
        t.reserve_at(&Attempt::sign_in("sam", address_keys(c), None), now)
            .unwrap_err()
            .1,
        Held::Address
    );
}

#[test]
fn a_hold_is_noted_at_its_first_refusal_and_again_after_the_next_failure() {
    let t = Throttle::default();
    let now = Instant::now();
    let owner = from("owner", A);
    fail(&t, &owner, now);
    assert!(
        t.reserve_inner(&owner, now).unwrap_err().2,
        "the hold's first refusal"
    );
    assert!(!t.reserve_inner(&owner, now).unwrap_err().2);
    assert!(!t.reserve_inner(&owner, now).unwrap_err().2);
    // The delay ran out and the next attempt failed: a new hold.
    let later = now + Duration::from_secs(2);
    fail(&t, &owner, later);
    assert!(t.reserve_inner(&owner, later).unwrap_err().2);
}
