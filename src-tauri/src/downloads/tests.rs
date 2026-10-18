use std::path::{Path, PathBuf};

use super::*;

fn env(home: &str, user_dirs: &str) -> DownloadEnv {
    DownloadEnv {
        home: Some(PathBuf::from(home)),
        user_dirs: user_dirs.to_string(),
    }
}

/// A scratch folder under the target dir, removed again by the test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target")
        .join(format!("lmgw-dl-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_taken_download_name_gets_a_counter_before_the_extension() {
    let dir = scratch("counter");
    let mut p = dir.join("lmgw-chat-1-x.md");
    unique_path(&mut p);
    assert_eq!(p, dir.join("lmgw-chat-1-x.md"), "a free name stays");
    std::fs::write(&p, "a").unwrap();
    std::fs::write(dir.join("lmgw-chat-1-x (1).md"), "b").unwrap();
    let mut again = p.clone();
    unique_path(&mut again);
    assert_eq!(again, dir.join("lmgw-chat-1-x (2).md"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_download_goes_to_the_downloads_folder_whatever_wry_proposed() {
    // No XDG_DOWNLOAD_DIR: wry proposes the working directory, and the
    // file still lands in ~/Downloads.
    let none = env("/home/u", "");
    assert_eq!(
        download_target(Path::new("/some/cwd/lmgw-chat-1-x.md"), &none).unwrap(),
        PathBuf::from("/home/u/Downloads/lmgw-chat-1-x.md")
    );
    // Configured, as xdg-user-dirs writes it.
    let conf = env(
        "/home/u",
        "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Herunterladen\"\n",
    );
    assert_eq!(
        download_target(Path::new("/home/u/Herunterladen/a.zip"), &conf).unwrap(),
        PathBuf::from("/home/u/Herunterladen/a.zip")
    );
    let abs = env("/home/u", "XDG_DOWNLOAD_DIR=\"/data/dl\"\n");
    assert_eq!(
        download_target(Path::new("x.json"), &abs).unwrap(),
        PathBuf::from("/data/dl/x.json")
    );
    // A relative value is not a folder to trust.
    let rel = env("/home/u", "XDG_DOWNLOAD_DIR=\"dl\"\n");
    assert_eq!(
        download_target(Path::new("x.json"), &rel).unwrap(),
        PathBuf::from("/home/u/Downloads/x.json")
    );
}

#[test]
fn a_download_keeps_only_its_file_name() {
    let e = env("/home/u", "");
    for proposed in [
        "/home/u/Downloads/../../etc/evil.md",
        "/etc/evil.md",
        "../../evil.md",
        "evil.md",
    ] {
        assert_eq!(
            download_target(Path::new(proposed), &e).unwrap(),
            PathBuf::from("/home/u/Downloads/evil.md"),
            "{proposed}"
        );
    }
    assert_eq!(
        download_target(Path::new("/home/u/Downloads/.."), &e).unwrap(),
        PathBuf::from("/home/u/Downloads/download")
    );
    let no_home = DownloadEnv {
        home: None,
        user_dirs: String::new(),
    };
    assert!(download_target(Path::new("a.md"), &no_home).is_err());
}

/// `is_dir` for a disk holding just `dirs`.
fn there(dirs: &'static [&'static str]) -> impl Fn(&Path) -> bool {
    move |p| dirs.iter().any(|d| p == Path::new(d))
}

#[test]
fn the_dialog_opens_in_the_folder_chosen_last_else_downloads_else_home() {
    let e = env("/home/u", "XDG_DOWNLOAD_DIR=\"$HOME/Herunterladen\"\n");
    let mut s = SaveSession::default();
    let first = s.start(
        "lmgw-lab-audio-2026-10-17.zip",
        &e,
        there(&["/home/u/Herunterladen"]),
    );
    assert_eq!(
        first,
        DialogStart {
            folder: Some(PathBuf::from("/home/u/Herunterladen")),
            name: "lmgw-lab-audio-2026-10-17.zip".into(),
        },
        "the first one: the Downloads folder, the server's name"
    );
    let dest = s.chosen(PathBuf::from("/home/u/Music/takes/000042-kokoro.wav"));
    assert_eq!(dest, PathBuf::from("/home/u/Music/takes/000042-kokoro.wav"));
    let all = there(&["/home/u/Herunterladen", "/home/u/Music/takes"]);
    assert_eq!(
        s.start("x.png", &e, all).folder,
        Some(PathBuf::from("/home/u/Music/takes")),
        "the next one: where the last one went"
    );
    assert_eq!(
        s.start("x.png", &e, there(&["/home/u/Herunterladen"]))
            .folder,
        Some(PathBuf::from("/home/u/Herunterladen")),
        "that folder gone: Downloads again"
    );
    assert_eq!(
        s.start("x.png", &e, there(&[])).folder,
        Some(PathBuf::from("/home/u")),
        "no Downloads folder: home"
    );
    let homeless = DownloadEnv {
        home: None,
        user_dirs: String::new(),
    };
    assert_eq!(
        SaveSession::default()
            .start("x.png", &homeless, there(&[]))
            .folder,
        None
    );
}

#[test]
fn the_dialogs_name_is_the_suggested_file_name_only() {
    let e = env("/home/u", "");
    let s = SaveSession::default();
    let name = |suggested: &str| s.start(suggested, &e, |_: &Path| true).name;
    assert_eq!(name("Save audio-kokoro.wav"), "Save audio-kokoro.wav");
    assert_eq!(name("../../etc/evil.wav"), "evil.wav");
    assert_eq!(name("/abs/path/x.zip"), "x.zip");
    assert_eq!(name(""), "download");
    assert_eq!(name(".."), "download");
}

#[test]
fn a_chosen_file_is_written_as_chosen_even_over_one_that_exists() {
    let dir = scratch("chosen");
    let taken = dir.join("take.wav");
    std::fs::write(&taken, "old").unwrap();
    let mut s = SaveSession::default();
    assert_eq!(
        s.chosen(taken.clone()),
        taken,
        "no counter: the dialog asked before replacing it"
    );
    // Only a download saved without a dialog steps around a taken name.
    let mut fallback = taken.clone();
    unique_path(&mut fallback);
    assert_eq!(fallback, dir.join("take (1).wav"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_cancel_says_nothing_and_a_failure_says_why() {
    let url = "http://127.0.0.1:8001/labs/api/audio/export?format=files";
    assert_eq!(notice(true, None, None, url), None, "the owner's Cancel");
    assert_eq!(
        notice(true, Some("Download cancelled by the user"), None, url),
        None
    );
    assert_eq!(
        notice(false, None, Some("/home/u/Music/a.zip"), url),
        Some(Notice::Saved("/home/u/Music/a.zip".into()))
    );
    assert_eq!(
        notice(false, Some("No space left on device"), Some("/x"), url),
        Some(Notice::Failed(format!(
            "{url} did not finish: No space left on device"
        )))
    );
    assert_eq!(
        notice(false, None, None, url),
        Some(Notice::Failed(format!("{url} did not finish")))
    );
    let js = Notice::Saved("/home/u/a \"b\".zip".into()).js();
    assert_eq!(
        js,
        "window.dispatchEvent(new CustomEvent(\"lmgw-downloaded\", \
         {detail: {\"path\":\"/home/u/a \\\"b\\\".zip\"}}));"
    );
}
