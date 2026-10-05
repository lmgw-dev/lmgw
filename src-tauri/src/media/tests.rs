use super::{
    decide, document_at_gateway, is_gateway_origin, mock_capture_requested, running_after, Ask,
    Decision, Load,
};
use crate::WindowOrigin;

fn origin() -> WindowOrigin {
    WindowOrigin::of(
        &"http://127.0.0.1:8001/api/session/login?nonce=x"
            .parse::<tauri::Url>()
            .unwrap(),
    )
}

#[test]
fn the_gateway_origin_is_its_exact_scheme_host_and_port() {
    let o = origin();
    let ok = |u: &str| is_gateway_origin(u, &o);
    assert!(ok("http://127.0.0.1:8001/chat/12"));
    assert!(ok("http://127.0.0.1:8001/"));
    assert!(ok("http://127.0.0.1:8001"));
    assert!(
        ok("HTTP://127.0.0.1:8001/x"),
        "schemes and hosts are case-blind"
    );
    assert!(
        !ok("http://board.localhost:8001/"),
        "an agent app's host: navigation_allowed admits it, the microphone does not"
    );
    assert!(
        !ok("http://localhost:8001/"),
        "another name for the same box"
    );
    assert!(!ok("http://[::1]:8001/"));
    assert!(!ok("http://127.0.0.2:8001/"));
    assert!(!ok("http://127.0.0.1:8002/"), "another port");
    assert!(!ok("http://127.0.0.1/"), "the default port is another port");
    assert!(!ok("https://127.0.0.1:8001/"), "another scheme");
    assert!(!ok("http://127.0.0.1:8001@evil.example/"), "userinfo trick");
    assert!(!ok("http://x@127.0.0.1:8001/"), "userinfo at all");
    assert!(!ok("about:blank"));
    assert!(!ok("about:srcdoc"), "the HTML preview frame's own URI");
    assert!(
        !ok("blob:http://127.0.0.1:8001/6f1b"),
        "a blob is not a document origin here"
    );
    assert!(!ok("data:text/html,x"));
    assert!(!ok("file:///etc/passwd"));
    assert!(!ok(""));
    assert!(!ok("not a url"));
}

#[test]
fn only_audio_at_the_gateway_is_granted() {
    let at = Ok(());
    let elsewhere = Err("not the gateway's origin");
    let moving = Err("navigation in progress");
    let media = |audio, video, display| Ask::UserMedia {
        audio,
        video,
        display,
    };
    assert_eq!(decide(media(true, false, false), at), Decision::Grant);
    assert_eq!(decide(Ask::DeviceInfo, at), Decision::Grant);

    for not_at in [elsewhere, moving] {
        let why = not_at.unwrap_err();
        assert_eq!(
            decide(media(true, false, false), not_at),
            Decision::Deny(why)
        );
        assert_eq!(decide(Ask::DeviceInfo, not_at), Decision::Deny(why));
    }
    assert_eq!(
        decide(media(false, true, false), at),
        Decision::Deny("video")
    );
    assert_eq!(
        decide(media(true, true, false), at),
        Decision::Deny("video"),
        "audio with video is refused whole"
    );
    assert_eq!(
        decide(media(false, true, true), at),
        Decision::Deny("display capture")
    );
    assert_eq!(
        decide(media(true, true, true), at),
        Decision::Deny("display capture")
    );
    assert_eq!(
        decide(media(false, false, false), at),
        Decision::Deny("no audio device asked for")
    );
    // Everything else is WebKit's default's to deny, wherever it comes from.
    assert_eq!(decide(Ask::Other, at), Decision::PassOn);
    assert_eq!(decide(Ask::Other, elsewhere), Decision::PassOn);
}

/// The running document's URI, the active URI, the answer, and the case.
type Row<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Result<(), &'a str>,
    &'a str,
);

/// Review M1: the active URI is the provisional one during a navigation, and
/// the old document runs until the new one commits. Both have to be the
/// gateway's.
#[test]
fn the_running_document_and_the_active_uri_must_both_be_the_gateway() {
    let o = origin();
    let gw = Some("http://127.0.0.1:8001/chat/12");
    let gw_other_path = Some("http://127.0.0.1:8001/settings");
    let agent = Some("http://board.localhost:8001/");
    let other_port = Some("http://127.0.0.1:8002/");
    let table: [Row; 9] = [
        (gw, gw, Ok(()), "the dashboard, idle"),
        (gw, gw_other_path, Ok(()), "a same-origin load in flight"),
        (
            agent,
            gw,
            Err("navigation in progress"),
            "an agent document navigating to the gateway (the M1 window)",
        ),
        (
            gw,
            agent,
            Err("navigation in progress"),
            "the dashboard on its way to an agent app",
        ),
        (
            None,
            gw,
            Err("navigation in progress"),
            "the first load, not committed yet",
        ),
        (
            agent,
            agent,
            Err("not the gateway's origin"),
            "an agent app's top-level document",
        ),
        (
            other_port,
            gw,
            Err("navigation in progress"),
            "another port navigating here",
        ),
        (None, None, Err("not the gateway's origin"), "no document"),
        (
            Some("about:blank"),
            None,
            Err("not the gateway's origin"),
            "a blank window",
        ),
    ];
    for (running, active, want, what) in table {
        assert_eq!(
            document_at_gateway(running, active, Some(&o)),
            want,
            "{what}"
        );
    }
}

/// WP11 review m3: the origin is what this process serves now. Nothing while
/// it serves nothing (a failed bind, a restart in between); after a restart
/// moved the port, the old origin's document is nobody's.
#[test]
fn the_document_is_judged_against_what_this_process_serves_now() {
    let gw = Some("http://127.0.0.1:8001/chat/12");
    assert_eq!(
        document_at_gateway(gw, gw, None),
        Err("the gateway is not serving"),
        "a failed bind: whoever holds the port gets nothing"
    );
    assert_eq!(
        document_at_gateway(None, None, None),
        Err("the gateway is not serving")
    );
    let moved = WindowOrigin::of(&"http://127.0.0.1:8002/".parse::<tauri::Url>().unwrap());
    assert_eq!(
        document_at_gateway(gw, gw, Some(&moved)),
        Err("not the gateway's origin"),
        "the port lmgw no longer owns"
    );
    let new = Some("http://127.0.0.1:8002/api/session/login?nonce=y");
    assert_eq!(
        document_at_gateway(gw, new, Some(&moved)),
        Err("navigation in progress"),
        "the window on its way to the new origin"
    );
    assert_eq!(document_at_gateway(new, new, Some(&moved)), Ok(()));
}

#[test]
fn the_running_document_moves_on_commit_and_on_an_idle_finish_only() {
    let doc = || Some("http://127.0.0.1:8001/".to_string());
    let next = "http://board.localhost:8001/";
    assert_eq!(
        running_after(Load::Provisional, Some(next), true, doc()),
        doc(),
        "a started or redirected load leaves the old document running"
    );
    assert_eq!(
        running_after(Load::Committed, Some(next), true, doc()).as_deref(),
        Some(next)
    );
    assert_eq!(
        running_after(Load::Finished, Some(next), false, None).as_deref(),
        Some(next),
        "a finish with nothing else loading is the document's own URI"
    );
    assert_eq!(
        running_after(Load::Finished, Some(next), true, doc()),
        doc(),
        "a finish reported after another load started names that load"
    );
}

#[test]
fn mock_devices_need_a_debug_build_and_the_value_1() {
    assert!(mock_capture_requested(Some("1"), true));
    assert!(!mock_capture_requested(Some("1"), false));
    assert!(!mock_capture_requested(Some("true"), true));
    assert!(!mock_capture_requested(Some(""), true));
    assert!(!mock_capture_requested(None, true));
}

/// Whether `line` gives a frame an `allow` attribute, in any case
/// (attribute names are case-blind): the word `allow` (not part of a longer
/// name such as `allow-scripts`) followed by `=` (not `==`) unless it is a
/// variable binding, or by `:` as an object key (`{allow: …}`,
/// `Object.assign(f, {…, allow: …})`); a `setAttribute("allow", …)`,
/// `setAttributeNS(…, "allow", …)` or d3-style `.attr("allow", …)` call; or
/// a bracketed property (`f["allow"] = …`).
fn delegates_a_permission(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    for q in ['"', '\'', '`'] {
        let calls = [
            format!("ttribute({q}allow{q}"),
            format!("attr({q}allow{q}"),
            format!("[{q}allow{q}]"),
        ];
        if calls.iter().any(|c| line.contains(c.as_str())) {
            return true;
        }
    }
    let quoted = ["\"allow\"", "'allow'", "`allow`"];
    let mut from = 0;
    while let Some(i) = line[from..].find("setattributens(") {
        let args_at = from + i + "setattributens(".len();
        let args = line[args_at..].split(')').next().unwrap_or_default();
        if quoted.iter().any(|q| args.contains(q)) {
            return true;
        }
        from = args_at;
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut from = 0;
    while let Some(i) = line[from..].find("allow") {
        let start = from + i;
        let end = start + "allow".len();
        from = end;
        if line[..start].chars().next_back().is_some_and(word)
            || line[end..].chars().next().is_some_and(word)
        {
            continue;
        }
        let rest = line[end..].trim_start();
        let before = line[..start].trim_end();
        if rest.starts_with('=') && !rest.starts_with("==") {
            if ["let", "mut", "const", "var"]
                .iter()
                .any(|kw| before.ends_with(kw))
            {
                continue;
            }
            return true;
        }
        if rest.starts_with(':')
            && !rest.starts_with("::")
            && (before.ends_with('{') || before.ends_with(','))
        {
            return true;
        }
    }
    false
}

#[test]
fn the_attribute_check_tells_a_delegation_from_its_neighbours() {
    for yes in [
        r#"<iframe src=src allow="microphone"></iframe>"#,
        r#"    allow="camera; microphone""#,
        r#"<iframe ALLOW="microphone">"#,
        r#"<iframe Allow = "microphone">"#,
        r#"<iframe attr:allow="microphone">"#,
        r#"frame.allow = "microphone";"#,
        r#"f.setAttribute("allow", "microphone")"#,
        r#"f.setAttribute("ALLOW", "microphone")"#,
        r#"f.setAttribute(`allow`, "microphone")"#,
        r#"f.setAttributeNS(null, "allow", "microphone")"#,
        r#"el.set_attribute("allow", "*")"#,
        r#"t.append("iframe").attr("allow","microphone")"#,
        r#"f["allow"] = "microphone";"#,
        r#"f['Allow'] = "microphone";"#,
        r#"Object.assign(f, {allow: "microphone"})"#,
        r#"Object.assign(f, { src, allow: "microphone" })"#,
    ] {
        assert!(delegates_a_permission(yes), "{yes}");
    }
    for no in [
        r#"sandbox="allow-scripts allow-modals allow-forms allow-popups""#,
        r#"if scope_mode.get() == "allow" {"#,
        r#"("allow".into(), "allow only…".into()),"#,
        r#"let allow = policy.allows(x);"#,
        r#"let mut allow = false;"#,
        r#"const allow = 1;"#,
        r#"allowed = true"#,
        r#"// never set allow_list"#,
        r#"disallow: Option<(Callback<CatalogEntry, bool>, &'static str)>,"#,
        r#"pub allow: bool,"#,
        r#"#[allow(clippy::too_many_arguments)]"#,
        r#"Policy::Allow::Yes => {}"#,
        r#"path.setAttributeNS(null, "d", shape)"#,
        r#"const modes = ["allow", "deny"];"#,
    ] {
        assert!(!delegates_a_permission(no), "{no}");
    }
}

/// §13.1: the handler cannot tell a frame from the top document, so the
/// dashboard must never delegate a permission to a frame — measured, an
/// `<iframe allow="microphone">` of another origin is granted the microphone
/// under the gateway's URI. The UI's sources, assets (vendored libraries
/// included) and page shell are all read.
#[test]
fn the_dashboard_never_delegates_a_frame_permission() {
    let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/lmgw-ui");
    let mut stack = vec![ui.join("src"), ui.join("assets"), ui.join("index.html")];
    let mut read = 0;
    let mut found = Vec::new();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
            continue;
        }
        // Binary assets (fonts, images) are not markup or script.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        read += 1;
        for (n, line) in text.lines().enumerate() {
            if delegates_a_permission(line) {
                found.push(format!("{}:{}", path.display(), n + 1));
            }
        }
    }
    assert!(
        read > 50,
        "the UI tree was not found under {}",
        ui.display()
    );
    assert!(
        found.is_empty(),
        "an `allow` attribute would hand a frame the gateway's microphone: {found:?}"
    );
}
