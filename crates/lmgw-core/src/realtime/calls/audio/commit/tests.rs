use super::*;

const MS: fn(u64) -> Duration = Duration::from_millis;

/// The defaults: a 120 ms commit window, a 60 ms reorder window (§9).
fn order() -> Order<&'static str> {
    Order::new(MS(120), MS(60))
}

#[test]
fn a_commit_takes_the_frames_stamped_within_its_window() {
    let t = Instant::now();
    let mut o = order();
    assert_eq!(o.event("commit", Cut::Commit, t), Vec::<&str>::new());
    assert_eq!(o.deadline(), Some(t + MS(180)));
    // The turn's last frames, captured before the release and stamped up to
    // 120 ms past it, still belong to it.
    for stamp in [MS(0), MS(50), MS(120)] {
        let (out, late) = o.frame(t + stamp, t + MS(10) + stamp);
        assert!(out.is_empty() && late.is_none(), "{stamp:?}");
    }
    // The first frame past the cut releases the commit, before it.
    let (out, late) = o.frame(t + MS(140), t + MS(150));
    assert_eq!((out, late), (vec!["commit"], None));
    assert_eq!(o.deadline(), None);
}

#[test]
fn a_commit_with_no_frame_past_its_cut_goes_after_both_windows() {
    let t = Instant::now();
    let mut o = order();
    o.event("commit", Cut::Commit, t);
    assert!(o.due(t + MS(179)).is_empty());
    assert_eq!(o.due(t + MS(180)), vec!["commit"]);
    // A frame of the turn that comes later still lands in the next one,
    // and says how late it was.
    let (out, late) = o.frame(t + MS(100), t + MS(200));
    assert!(out.is_empty());
    let late = late.unwrap();
    assert_eq!(
        late,
        LateFrame {
            cut: Cut::Commit,
            before_cut: MS(20),
            after_applied: MS(20)
        }
    );
    assert!(
        late.to_string()
            .contains("realtime.webrtc_commit_window_ms"),
        "{late}"
    );
    // A frame of the next turn is not late.
    assert_eq!(o.frame(t + MS(130), t + MS(210)).1, None);
}

#[test]
fn events_behind_a_waiting_commit_wait_with_it_and_others_go_at_once() {
    let t = Instant::now();
    let mut o = order();
    assert_eq!(o.event("cancel", Cut::Other, t), vec!["cancel"]);
    o.event("commit", Cut::Commit, t + MS(10));
    assert!(o
        .event("response.create", Cut::Other, t + MS(15))
        .is_empty());
    assert!(o.event("update", Cut::Other, t + MS(20)).is_empty());
    let (out, _) = o.frame(t + MS(200), t + MS(205));
    assert_eq!(out, vec!["commit", "response.create", "update"]);
    assert_eq!(o.event("cancel", Cut::Other, t + MS(210)), vec!["cancel"]);
}

#[test]
fn a_clear_cuts_at_its_own_stamp_and_waits_only_for_the_reorder_window() {
    let t = Instant::now();
    let mut o = order();
    o.event("clear", Cut::Clear, t);
    assert_eq!(o.deadline(), Some(t + MS(60)));
    // A frame captured before the press goes first; the first one after it
    // releases the clear.
    assert!(o.frame(t - MS(5), t + MS(20)).0.is_empty());
    assert_eq!(o.frame(t + MS(15), t + MS(40)).0, vec!["clear"]);
    // With nothing after it, the reorder window ends the wait.
    o.event("clear", Cut::Clear, t + MS(100));
    assert!(o.due(t + MS(159)).is_empty());
    assert_eq!(o.due(t + MS(160)), vec!["clear"]);
    // With the reorder window off, a clear goes at once.
    let mut off = Order::new(MS(120), Duration::ZERO);
    assert_eq!(off.event("clear", Cut::Clear, t), vec!["clear"]);
}

#[test]
fn the_end_applies_whatever_waits_at_once() {
    let t = Instant::now();
    let mut o = order();
    o.event("stop", Cut::Other, t);
    o.event("commit", Cut::Commit, t + MS(1));
    o.event("close", Cut::Other, t + MS(2));
    assert_eq!(o.flush(), vec!["commit", "close"]);
    assert_eq!(o.deadline(), None);
}
