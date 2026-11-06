"""CLI of the live check; scripts/compat-live.sh runs it in a container (see its --help)."""

import argparse
import datetime
import os
import sys
import time

from . import report, runner


def _key():
    k = os.environ.get("LMGW_API_KEY", "").strip()
    if k:
        return k
    p = os.path.expanduser("~/.config/lmgw/compat-key")
    if os.path.isfile(p):
        with open(p) as f:
            return f.read().strip()
    return ""


def _list(s):
    return [x.strip() for x in s.split(",") if x.strip()] if s else []


def main(argv=None):
    ap = argparse.ArgumentParser(prog="compat-live", description=__doc__)
    ap.add_argument("--base", default="http://127.0.0.1:8001", help="lmgw's base URL")
    ap.add_argument("--models", help="comma-separated aliases (overrides --preset)")
    ap.add_argument("--preset", default="default", choices=sorted(runner.PRESETS))
    ap.add_argument("--routes", default=",".join(runner.ROUTES), help="chat,messages,responses")
    ap.add_argument("--scenarios", default="", help=f"subset of: {','.join(runner.SCENARIO_IDS)}")
    ap.add_argument("--modes", default="plain,stream")
    ap.add_argument("--jobs", type=int, default=6, help="cells run at once")
    ap.add_argument("--timeout", type=float, default=180.0, help="seconds per request")
    ap.add_argument("--out", default="target/compat/runs", help="the runs directory")
    ap.add_argument("--list", action="store_true", help="print the scenario catalogue and exit")
    a = ap.parse_args(argv)
    if a.list:
        from .scenarios import CATALOGUE
        for s in CATALOGUE:
            print(f"{s.id:22} {'/'.join(s.routes):26} {'/'.join(s.modes):12} {s.title}")
        return 0
    key = _key()
    if not key:
        print("compat-live: no key: set LMGW_API_KEY or write ~/.config/lmgw/compat-key (mode 600)", file=sys.stderr)
        return 2
    models = _list(a.models) or runner.PRESETS[a.preset]
    routes, modes = _list(a.routes), _list(a.modes)
    scen = _list(a.scenarios) or runner.SCENARIO_IDS
    bad = [r for r in routes if r not in runner.ROUTES] + [s for s in scen if s not in runner.SCENARIO_IDS] \
        + [m for m in modes if m not in ("plain", "stream")]
    if bad:
        print(f"compat-live: unknown route/scenario/mode: {bad}", file=sys.stderr)
        return 2
    base = a.base.rstrip("/")
    started = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = os.path.join(a.out, started)
    os.makedirs(run_dir, exist_ok=True)
    t0 = time.monotonic()

    def progress(n, total, r):
        mark = {"pass": ".", "fail": "F", "unsupported": "u", "skipped": "s", "model": "m"}[r["status"]]
        line = f"[{n}/{total}] {mark} {r['key']}"
        if r["status"] != "pass":
            line += f"  {r['reason'][:160]}"
        print(line, flush=True)

    results, cards, requests = runner.run(base, key, models, routes, scen, modes, a.jobs, a.timeout, progress)
    usd, unpriced = runner.cost(results, cards)
    cost_note = f"about ${usd:.4f}, estimated from the answers' usage and the per-token prices on GET /v1/models"
    if unpriced:
        cost_note += f"; no price on /v1/models for {', '.join(unpriced)} (not counted)"
    wanted = {"models": models, "routes": routes, "modes": modes, "scenarios": scen}
    prev_name, prev, partial = report.previous_run(a.out, started, wanted)
    d = report.diff(prev, results, wanted)
    meta = {"started": started, "base": base, "models": models, "routes": routes, "modes": modes,
            "scenarios": scen, "requests": requests, "cost_note": cost_note, "cost_usd": round(usd, 6),
            "unpriced_models": unpriced, "totals": report.totals(results), "previous": prev_name,
            "previous_partial": list(partial) if partial else None,
            "seconds": round(time.monotonic() - t0, 1)}
    report.write_json(os.path.join(run_dir, "results.json"), meta, results, d)
    report.write_html(os.path.join(run_dir, "report.html"), meta, results, d)

    print("\nroute x model:")
    for (route, model), c in sorted(report.counts(results).items()):
        print(f"  {route:10} {model:40} " + ", ".join(f"{s} {c[s]}" for s in c))
    rules = report.by_rule(results)
    if rules:
        print("\nfailures by rule (cells):")
        for rule, keys in rules:
            print(f"  {len(keys):4}  {rule}")
    print(f"\ntotals: {meta['totals']}")
    print(f"requests: {requests}")
    print(f"cost: {cost_note}")
    print(f"against {report.baseline(prev_name, partial)}: {len(d['new_failures'])} new failure(s), "
          f"{len(d['fixed'])} fixed, {len(d['changed'])} other change(s)")
    host_dir = os.environ.get("COMPAT_HOST_RUNS")
    shown = os.path.join(host_dir, started) if host_dir else run_dir
    print(f"report: {os.path.join(shown, 'report.html')}")
    return 1 if any(r["status"] == "fail" for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
