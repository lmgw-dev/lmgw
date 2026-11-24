//! Settings › Documents' "Programs run confined" (performance round review
//! S5): whether lmgw's document reader confines the tools it starts here,
//! and each found tool that runs without confinement, with why. Shown above
//! the found tools, in plain words for everyone.

use leptos::prelude::*;
use lmgw_api_types::documents::{Confinement, ToolRow};

/// The row's value and its sentence.
pub(super) fn said(c: &Confinement, tools: &[ToolRow]) -> (&'static str, String) {
    let unconfined: Vec<String> = tools
        .iter()
        .filter_map(|t| {
            t.not_confined
                .as_ref()
                .map(|why| format!("{} runs without it: {why}", t.label))
        })
        .collect();
    match &c.why_not {
        Some(why) => (
            "No",
            format!(
                "The tools lmgw starts run with their memory limit alone, not confined, because \
                 {why}; LibreOffice is not used."
            ),
        ),
        None => {
            let mut s = "Each tool lmgw starts reaches only the files it needs".to_string();
            if let Some(abi) = c.abi {
                s.push_str(&format!(" (Landlock {abi})"));
            }
            // What this kernel's Landlock cannot hold them to (M3).
            if !c.lacks.is_empty() {
                s.push_str(". For every tool, this kernel leaves open: ");
                s.push_str(&c.lacks.join("; "));
            }
            for u in &unconfined {
                s.push_str(". ");
                s.push_str(u);
            }
            if !s.ends_with('.') {
                s.push('.');
            }
            ("Yes", s)
        }
    }
}

/// "Programs run confined": yes or no, and why.
#[component]
pub(super) fn ConfinedRow(confinement: Confinement, tools: Vec<ToolRow>) -> impl IntoView {
    let (value, sub) = said(&confinement, &tools);
    let warn = value == "No"
        || !confinement.lacks.is_empty()
        || tools.iter().any(|t| t.not_confined.is_some());
    view! {
        <div class="prow doc-row" data-part="confined">
            <div class="prow-text">
                <span class="prow-title">"Programs run confined"</span>
                <span class="prow-sub" class:doc-warn=warn>{sub}</span>
            </div>
            <span class="prow-value">{value}</span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::documents::ToolState;

    fn tool(label: &str, why: Option<&str>) -> ToolRow {
        ToolRow {
            id: "poppler".into(),
            label: label.into(),
            state: ToolState::Found,
            version: None,
            path: None,
            extras: Vec::new(),
            message: None,
            licence: String::new(),
            used_for: Vec::new(),
            checked_at: 0,
            not_confined: why.map(str::to_string),
        }
    }

    #[test]
    fn it_says_yes_and_names_each_tool_that_runs_without_it() {
        let c = Confinement {
            abi: Some(11),
            why_not: None,
            lacks: Vec::new(),
        };
        let tools = [
            tool(
                "PDF tools (poppler)",
                Some("it is installed in /opt/example"),
            ),
            tool("Pandoc", None),
        ];
        let (v, s) = said(&c, &tools);
        assert_eq!(v, "Yes");
        assert_eq!(
            s,
            "Each tool lmgw starts reaches only the files it needs (Landlock 11). PDF tools \
             (poppler) runs without it: it is installed in /opt/example."
        );
        let off = Confinement {
            abi: None,
            why_not: Some("this kernel has no Landlock".into()),
            lacks: Vec::new(),
        };
        let (v, s) = said(&off, &tools);
        assert_eq!(v, "No");
        assert!(s.contains("because this kernel has no Landlock"), "{s}");
        // An older kernel: each protection it lacks, with the ABI found.
        let old = Confinement {
            abi: Some(5),
            why_not: None,
            lacks: vec![
                "abstract Unix sockets and signals to other processes are not blocked (that \
                 needs Landlock ABI 6; this kernel has 5)"
                    .into(),
                "UDP is not blocked (that needs Landlock ABI 10; this kernel has 5)".into(),
            ],
        };
        let (v, s) = said(&old, &[]);
        assert_eq!(v, "Yes");
        assert!(
            s.contains("(Landlock 5). For every tool, this kernel leaves open: abstract Unix"),
            "{s}"
        );
        assert!(
            s.contains("; UDP is not blocked (that needs Landlock ABI 10"),
            "{s}"
        );
    }
}
