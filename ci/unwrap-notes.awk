# Join the wrapped lines of a Markdown text into one line per paragraph or list
# item. GitHub shows every line break of a release description, so a tag
# message wrapped at 72 columns reads as ragged lines on a narrow screen.
# Fenced code blocks, headings, quotes and tables keep their lines.
#
# Usage: awk -f ci/unwrap-notes.awk < notes.md
function flush() {
    if (buf != "") print buf
    buf = ""
}
/^[[:space:]]*```/ { flush(); print; fence = !fence; next }
fence { print; next }
/^[[:space:]]*$/ { flush(); print ""; next }
/^[[:space:]]*(#|>|\|)/ { flush(); print; next }
/^[[:space:]]*([-*+]|[0-9]+[.)])[[:space:]]/ { flush(); buf = $0; next }
{
    if (buf == "") { buf = $0; next }
    line = $0
    sub(/^[[:space:]]+/, "", line)
    buf = buf " " line
}
END { flush() }
