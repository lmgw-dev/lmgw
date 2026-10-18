//! What a kept lab says about its size (labs-store design §1.5, §2.1): the
//! results head's "kept · 1 234 runs · 2 set runs · 2.1 MB", its tooltip
//! split into text and files, and the words Clear asks with. The runs are
//! every run the lab keeps, its set runs' items' among them (§3.5). Pure,
//! so the counts read the same in every lab and the tests pin them.

use lmgw_api_types::lab_store::LabUsage;

use crate::fmt::{grouped, human_bytes};

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", grouped(n), if n == 1 { one } else { many })
}

/// " · 2 set runs" when the lab holds any, else nothing.
fn set_runs(u: &LabUsage) -> String {
    if u.batches == 0 {
        String::new()
    } else {
        format!(" · {}", plural(u.batches, "set run", "set runs"))
    }
}

/// The results head's note: "newest first · kept · 48 runs · 2 set runs ·
/// 21.4 kB", and the files when the lab keeps any ("· 3 files · 4.1 MB").
/// `None` before the usage was read: the size is not guessed.
pub(in crate::pages) fn kept_note(u: Option<&LabUsage>) -> String {
    match u {
        None => "newest first · kept".into(),
        Some(u) if u.artifact_files > 0 => format!(
            "newest first · kept · {}{} · {} · {}",
            plural(u.runs, "run", "runs"),
            set_runs(u),
            plural(u.artifact_files, "file", "files"),
            human_bytes(u.text_bytes + u.artifact_bytes)
        ),
        Some(u) => format!(
            "newest first · kept · {}{} · {}",
            plural(u.runs, "run", "runs"),
            set_runs(u),
            human_bytes(u.text_bytes + u.artifact_bytes)
        ),
    }
}

/// The note's tooltip: the size split into stored text and files, and the
/// orphan files when there are any.
pub(in crate::pages) fn kept_title(u: Option<&LabUsage>) -> String {
    let Some(u) = u else {
        return "Every group is kept on the server until it is deleted".into();
    };
    let mut out = format!(
        "{}{} · {} · {}\ntext {} (requests, answers, forms)\nfiles {} in {}",
        plural(u.groups, "group", "groups"),
        set_runs(u),
        plural(u.runs, "run", "runs"),
        plural(u.artifacts, "artifact", "artifacts"),
        human_bytes(u.text_bytes),
        human_bytes(u.artifact_bytes),
        plural(u.artifact_files, "file", "files"),
    );
    if u.orphan_files > 0 {
        out.push_str(&format!(
            "\n{} in the artifacts folder that no row names ({})",
            plural(u.orphan_files, "file", "files"),
            human_bytes(u.orphan_bytes)
        ));
    }
    out
}

/// What Clear asks before it deletes a lab's history: the runs, the set
/// runs and the files it takes with them (the question sets stay).
pub(in crate::pages) fn clear_confirm(u: Option<&LabUsage>, groups: usize) -> String {
    let Some(u) = u else {
        return format!("Delete all {}?", plural(groups as u64, "group", "groups"));
    };
    let mut parts = vec![plural(u.runs, "run", "runs")];
    if u.batches > 0 {
        parts.push(plural(u.batches, "set run", "set runs"));
    }
    if u.artifact_files > 0 {
        parts.push(plural(u.artifact_files, "file", "files"));
    }
    let last = parts.pop().unwrap_or_default();
    if parts.is_empty() {
        format!("Delete all {last}?")
    } else {
        format!("Delete all {} and {last}?", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::lab_store::LabKind;

    fn usage(runs: u64, files: u64) -> LabUsage {
        LabUsage {
            lab: LabKind::Decision,
            groups: 3,
            batches: 0,
            runs,
            artifacts: files,
            artifact_files: files,
            text_bytes: 20_000,
            artifact_bytes: 1_400_000,
            orphan_files: 0,
            orphan_bytes: 0,
        }
    }

    #[test]
    fn the_note_counts_runs_and_the_whole_size() {
        assert_eq!(
            kept_note(Some(&usage(1234, 2))),
            "newest first · kept · 1\u{202F}234 runs · 2 files · 1.4 MB"
        );
        assert_eq!(
            kept_note(Some(&usage(1, 0))),
            "newest first · kept · 1 run · 1.4 MB"
        );
        assert_eq!(
            kept_note(Some(&usage(1, 1))),
            "newest first · kept · 1 run · 1 file · 1.4 MB"
        );
        assert_eq!(kept_note(None), "newest first · kept");
        let mut u = usage(80, 0);
        u.batches = 2;
        assert_eq!(
            kept_note(Some(&u)),
            "newest first · kept · 80 runs · 2 set runs · 1.4 MB"
        );
        assert!(kept_title(Some(&u)).starts_with("3 groups · 2 set runs · 80 runs"));
    }

    #[test]
    fn the_tooltip_splits_text_and_files_and_names_orphans() {
        let mut u = usage(6, 2);
        assert!(!kept_title(Some(&u)).contains("no row names"));
        u.orphan_files = 1;
        u.orphan_bytes = 512;
        let t = kept_title(Some(&u));
        assert!(t.contains("text 20.0 kB"), "{t}");
        assert!(t.contains("files 1.4 MB in 2 files"), "{t}");
        assert!(
            t.contains("1 file in the artifacts folder that no row names (512 B)"),
            "{t}"
        );
    }

    #[test]
    fn clear_names_the_runs_and_the_files() {
        assert_eq!(
            clear_confirm(Some(&usage(48, 2)), 9),
            "Delete all 48 runs and 2 files?"
        );
        assert_eq!(clear_confirm(Some(&usage(1, 0)), 1), "Delete all 1 run?");
        assert_eq!(clear_confirm(None, 2), "Delete all 2 groups?");
        let mut u = usage(48, 2);
        u.batches = 1;
        assert_eq!(
            clear_confirm(Some(&u), 9),
            "Delete all 48 runs, 1 set run and 2 files?"
        );
        u.artifact_files = 0;
        assert_eq!(
            clear_confirm(Some(&u), 9),
            "Delete all 48 runs and 1 set run?"
        );
    }
}
