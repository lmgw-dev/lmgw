//! The saved row's command line, one option per line.

/// Split a rendered command line at its whitespace, keeping quoted parts
/// (`'lmgw.instance=lmgw-dev'`) whole, quotes included.
fn shell_tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => {
                quote = Some(c);
                start.get_or_insert(i);
            }
            (None, c) if c.is_whitespace() => {
                if let Some(b) = start.take() {
                    out.push(&s[b..i]);
                }
            }
            (None, _) => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(b) = start {
        out.push(&s[b..]);
    }
    out
}

/// A token that is an option, not a value: `-1` is a value.
fn is_flag(t: &str) -> bool {
    t.starts_with('-')
        && t.chars()
            .nth(1)
            .is_some_and(|c| !c.is_ascii_digit() && c != '.')
}

/// `podman run … image llama-server-args` one option per line, each with its
/// value, so a flag can be found by eye instead of in one 900-character line.
/// A bare word after an option that already has its value (the image) gets a
/// line of its own. Podman's own options before the subcommand
/// (`--cdi-spec-dir <dir>`, lmgw's spec of the graphics card) stay on the
/// first line, between `podman` and `run`: they are the command, not the
/// container's options.
pub(super) fn flag_lines(cmd: &str) -> String {
    let tokens = shell_tokens(cmd);
    let head = command_head(&tokens);
    let mut lines: Vec<String> = Vec::new();
    if head > 0 {
        lines.push(tokens[..head].join(" "));
    }
    let mut cur = String::new();
    // The current line is an option still waiting for its value.
    let mut open = false;
    for &t in &tokens[head..] {
        let flag = is_flag(t);
        if flag || (!open && !cur.is_empty() && lines_started(&lines, &cur)) {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur.push_str(t);
            open = flag;
            continue;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(t);
        open = false;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

/// How many leading tokens are `podman`, its global options (each with its
/// value: `--opt value` or `--opt=value`) and the subcommand; 0 when the line
/// does not start with `podman` and a global option.
fn command_head(tokens: &[&str]) -> usize {
    if tokens.first() != Some(&"podman") || !tokens.get(1).is_some_and(|t| t.starts_with("--")) {
        return 0;
    }
    let mut i = 1;
    while let Some(t) = tokens.get(i).filter(|t| t.starts_with("--")) {
        i += if t.contains('=') { 1 } else { 2 };
    }
    // The subcommand.
    (i + 1).min(tokens.len())
}

/// Before the first option the words are the command itself ("podman run"):
/// they share a line. After it, a bare word that follows a complete option
/// starts its own.
fn lines_started(lines: &[String], cur: &str) -> bool {
    !lines.is_empty() || is_flag(cur.split(' ').next().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::{flag_lines, shell_tokens};

    #[test]
    fn quoted_parts_stay_one_token() {
        assert_eq!(
            shell_tokens("podman run --label 'a=b c' -v \"x y\":/m"),
            ["podman", "run", "--label", "'a=b c'", "-v", "\"x y\":/m"]
        );
    }

    #[test]
    fn podmans_own_options_stay_on_the_podman_line() {
        let cmd = "podman --cdi-spec-dir /run/user/1000/lmgw-machine/lmgw/cdi run -d \
                   --device nvidia.com/gpu=all img -m /models/x.gguf";
        assert_eq!(
            flag_lines(cmd),
            "podman --cdi-spec-dir /run/user/1000/lmgw-machine/lmgw/cdi run\n-d\n\
             --device nvidia.com/gpu=all\nimg\n-m /models/x.gguf"
        );
        assert_eq!(
            flag_lines("podman --cdi-spec-dir=/a --cdi-spec-dir /b run --name x img"),
            "podman --cdi-spec-dir=/a --cdi-spec-dir /b run\n--name x\nimg"
        );
    }

    #[test]
    fn a_command_line_reads_one_option_per_line() {
        let cmd = "podman run -d --replace --label 'lmgw.class=chat' -p 0:8080 \
                   localhost/llama:latest -m /models/x.gguf --jinja --seed -1 --temp 0.15";
        assert_eq!(
            flag_lines(cmd),
            "podman run\n-d\n--replace\n--label 'lmgw.class=chat'\n-p 0:8080\n\
             localhost/llama:latest\n-m /models/x.gguf\n--jinja\n--seed -1\n--temp 0.15"
        );
    }
}
