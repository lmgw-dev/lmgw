"""results.json, the diff against the newest earlier run that covers this one, and
report.html (a local file)."""

import html
import json
import os
import re
from collections import Counter

from .common import STATUSES

# The keys of results.json's `run` object that say what a run covered.
COVERAGE = ("models", "routes", "modes", "scenarios")


def covers(run, wanted):
    """Whether `run` (a results.json `run` object) covered every model, route, mode and
    scenario in `wanted` (the same keys); a key it lacks covers nothing."""
    return all(k in run and set(wanted.get(k) or ()) <= set(run[k] or ()) for k in COVERAGE)


def cells(run, wanted):
    """How many of `wanted`'s cells (models x routes x modes x scenarios) `run` covered too;
    a key it lacks covers nothing."""
    n = 1
    for k in COVERAGE:
        n *= len(set(wanted.get(k) or ()) & set(run.get(k) or ()))
    return n


def previous_run(runs_dir, current, wanted):
    """The baseline for the current run: `(name, results, partial)`. The newest results.json
    under runs_dir from before the current run whose run covers `wanted` (`covers`), so a rerun
    of a few cells is never the next full run's baseline; `partial` is None then. When none
    covers it, the newest earlier run sharing the most cells with it (`cells`), and `partial`
    is `(shared cells, this run's cells)`; `(None, None, None)` when no earlier run shares a
    cell."""
    try:
        names = sorted(n for n in os.listdir(runs_dir) if n < current)
    except FileNotFoundError:
        return None, None, None
    total = cells(wanted, wanted)
    best = (0, None, None)
    for n in reversed(names):
        p = os.path.join(runs_dir, n, "results.json")
        if not os.path.isfile(p):
            continue
        with open(p) as f:
            prev = json.load(f)
        run = prev.get("run") or {}
        if covers(run, wanted):
            return n, prev, None
        shared = cells(run, wanted)
        if shared > best[0]:
            best = (shared, n, prev)
    shared, n, prev = best
    if n is None:
        return None, None, None
    return n, prev, (shared, total)


def baseline(name, partial):
    """The words for the run a diff is against (`previous_run`'s name and partial)."""
    if name is None:
        return "no earlier run sharing a cell with this one"
    if partial is None:
        return name
    return f"{name} (a partial baseline: covers {partial[0]} of {partial[1]} cells)"


def diff(prev, now, wanted=None):
    """New failures, fixed cells and other status changes, by cell key. With `wanted` (what
    this run covers, `COVERAGE`'s keys), `gone` lists only the earlier run's cells inside it:
    the cells a larger earlier run had and this one did not ask for are not gone. A model-free
    cell's model is none of the earlier run's models, so only its scenario, mode and route
    count."""
    records = (prev or {}).get("results", [])
    if wanted is not None:
        models = set((prev or {}).get("run", {}).get("models") or ())

        def asked(r):
            return (r.get("scenario") in wanted["scenarios"] and r.get("mode") in wanted["modes"]
                    and r.get("route") in wanted["routes"]
                    and (r.get("model") in wanted["models"] or r.get("model") not in models))

        gone_from = {r["key"] for r in records if asked(r)}
    else:
        gone_from = {r["key"] for r in records}
    before = {r["key"]: r["status"] for r in records}
    out = {"new_failures": [], "fixed": [], "changed": [], "gone": []}
    keys_now = set()
    for r in now:
        k, s = r["key"], r["status"]
        keys_now.add(k)
        b = before.get(k)
        if s == "fail" and b != "fail":
            out["new_failures"].append({"key": k, "was": b, "reason": r["reason"]})
        elif b == "fail" and s == "pass":
            out["fixed"].append({"key": k})
        elif b is not None and b != s:
            out["changed"].append({"key": k, "was": b, "now": s})
    out["gone"] = sorted(k for k in gone_from if k not in keys_now)
    return out


def counts(results):
    """{(route, model): Counter(status)}."""
    c = {}
    for r in results:
        c.setdefault((r["route"], r["model"]), Counter())[r["status"]] += 1
    return c


def rule_of(violation):
    """The rule id of a violation: 'turn 2: chat.sse.done_last: ...' -> 'chat.sse.done_last'."""
    m = re.match(r"(?:turn \d+: )?([A-Za-z_.]+):", violation)
    return m.group(1) if m else violation[:40]


def by_rule(results):
    """[(rule, [cell keys])], the rule broken in the most cells first."""
    g = {}
    for r in results:
        for v in r.get("violations") or []:
            keys = g.setdefault(rule_of(v), [])
            if r["key"] not in keys:
                keys.append(r["key"])
    return sorted(g.items(), key=lambda kv: (-len(kv[1]), kv[0]))


def write_json(path, meta, results, d):
    with open(path, "w") as f:
        json.dump({"run": meta, "diff": d, "results": results}, f, indent=1, sort_keys=False)


CSS = """
body{font:14px/1.45 system-ui,sans-serif;background:#232629;color:#eff0f1;margin:24px}
h1,h2{font-weight:600}h2{margin-top:28px;border-bottom:1px solid #4d4d4d;padding-bottom:4px}
table{border-collapse:collapse;margin:8px 0}th,td{border:1px solid #4d4d4d;padding:4px 8px;text-align:left}
th{background:#31363b}td.c a{color:inherit;text-decoration:none;display:block}
.pass{background:#1f4d2e}.fail{background:#6b1f24}.unsupported{background:#3d4a5c}
.skipped{background:#3a3a3a;color:#bbb}.model{background:#5c4a1f}.na{color:#777}
pre{background:#1b1e20;padding:8px;overflow:auto;max-height:420px;white-space:pre-wrap}
details{margin:6px 0;border:1px solid #4d4d4d;padding:4px 8px}summary{cursor:pointer}
code{color:#3daee9}.meta td{border:none;padding:1px 8px}
"""


def _cell(r):
    if r is None:
        return '<td class="na">n/a</td>'
    e = html.escape
    return (f'<td class="c {r["status"]}" title="{e(r["reason"])}">'
            f'<a href="#{e(r["key"])}">{r["status"]}</a></td>')


def write_html(path, meta, results, d):
    e = html.escape
    by_key = {r["key"]: r for r in results}
    models = [m for m in meta["models"]]
    rows = []
    rows.append(f"<h1>lmgw API compatibility, live run {e(meta['started'])}</h1>")
    rows.append('<table class="meta">' + "".join(
        f"<tr><td>{e(k)}</td><td>{e(str(v))}</td></tr>" for k, v in (
            ("base", meta["base"]), ("models", ", ".join(models)), ("routes", ", ".join(meta["routes"])),
            ("requests", meta["requests"]), ("cost", meta["cost_note"]), ("totals", meta["totals"]),
            ("compared with", baseline(meta.get("previous"), meta.get("previous_partial"))),
            ("seconds", meta["seconds"]))) + "</table>")
    if meta.get("previous_partial"):
        rows.append("<h2>Against the earlier run sharing the most cells with this one "
                    "(a partial baseline)</h2>")
    else:
        rows.append("<h2>Against the newest earlier run that covers this one</h2>")
    for k, title in (("new_failures", "New failures"), ("fixed", "Fixed"), ("changed", "Other changes")):
        items = d[k]
        rows.append(f"<p><b>{title}</b>: {len(items)}</p>")
        if items:
            rows.append("<ul>" + "".join(
                f'<li><a href="#{e(i["key"])}"><code>{e(i["key"])}</code></a> '
                f'{e(str(i.get("was", "")))} &rarr; {e(str(i.get("now", i.get("reason", ""))))}</li>'
                for i in items) + "</ul>")
    rows.append("<h2>Failures by rule</h2><table><tr><th>rule</th><th>cells</th><th>first cells</th></tr>")
    for rule, keys in by_rule(results):
        links = " ".join(f'<a href="#{e(k)}"><code>{e(k)}</code></a>' for k in keys[:6])
        more = f" and {len(keys) - 6} more" if len(keys) > 6 else ""
        rows.append(f"<tr><td><code>{e(rule)}</code></td><td>{len(keys)}</td><td>{links}{more}</td></tr>")
    rows.append("</table>")
    scen = []
    for r in results:
        if (r["scenario"], r["mode"]) not in scen:
            scen.append((r["scenario"], r["mode"]))
    for route in meta["routes"]:
        rows.append(f"<h2>{e(route)}</h2><table><tr><th>scenario</th><th>mode</th>"
                    + "".join(f"<th>{e(m)}</th>" for m in models) + "<th>model-free</th></tr>")
        for sid, mode in scen:
            cells = [by_key.get(f"{sid}|{mode}|{route}|{m}") for m in models]
            free = [r for r in results if r["scenario"] == sid and r["mode"] == mode and r["route"] == route
                    and r["model"] not in models]
            if not any(cells) and not free:
                continue
            rows.append(f"<tr><td>{e(sid)}</td><td>{e(mode)}</td>" + "".join(_cell(c) for c in cells)
                        + (_cell(free[0]) if free else '<td class="na"></td>') + "</tr>")
        rows.append("</table>")
    rows.append("<h2>Details of every cell that did not pass</h2>")
    for r in results:
        if r["status"] == "pass":
            continue
        rows.append(f'<details id="{e(r["key"])}"><summary class="{r["status"]}"><code>{e(r["key"])}</code> '
                    f'{r["status"]}: {e(r["reason"][:200])}</summary>')
        for label, key in (("violations", "violations"), ("model behaviour", "model_notes"), ("notes", "notes")):
            if r.get(key):
                rows.append(f"<p>{label}</p><pre>{e(chr(10).join(r[key]))}</pre>")
        for n, ex in enumerate(r.get("evidence") or [], 1):
            rows.append(f"<p>request {n}: <code>{e(ex['request'])}</code> &rarr; {ex['status']} "
                        f"({ex['seconds']} s) {e(json.dumps(ex['lmgw_headers']))}</p>"
                        f"<pre>{e(ex['request_body'])}</pre><pre>{e(ex['response_body'])}</pre>")
        rows.append("</details>")
    with open(path, "w") as f:
        f.write(f"<!doctype html><meta charset=utf-8><title>lmgw compat {e(meta['started'])}</title>"
                f"<style>{CSS}</style>" + "\n".join(rows))


def totals(results):
    c = Counter(r["status"] for r in results)
    return ", ".join(f"{s} {c[s]}" for s in STATUSES if c[s])
