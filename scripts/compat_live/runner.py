"""Walks the matrix: scenario x mode x route x model, a few cells at a time."""

import time
import traceback
from concurrent.futures import ThreadPoolExecutor

import httpx2 as httpx  # the SDKs (openai 3, anthropic 2) are built on httpx2

from . import route_chat, route_messages, route_responses
from .cell import Cell
from .common import FAIL, SKIPPED, Outcome
from .scenarios import BY_ID, CATALOGUE, UNKNOWN_MODEL
from .transport import Counter, Kit

ROUTES = {"chat": route_chat, "messages": route_messages, "responses": route_responses}

# The preset follows the providers' current generation and gets raised when one ships a
# new one; dated ids, never the drifting *-latest aliases, so runs stay comparable.
PRESETS = {
    "default": ["kilo/anthropic/claude-haiku-5.5", "kilo/openai/gpt-6-luna",
                "kilo/google/gemini-3.8-flash", "anthropic/claude-haiku-5-5"],
    "kilo": ["kilo/anthropic/claude-haiku-5.5", "kilo/openai/gpt-6-luna", "kilo/google/gemini-3.8-flash"],
    # Bills the direct provider keys; run it on purpose only.
    "direct": ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna", "aistudio/gemini-3.8-flash"],
}


def fetch_cards(base, key, timeout):
    r = httpx.get(base + "/v1/models", headers={"authorization": f"Bearer {key}"}, timeout=timeout)
    r.raise_for_status()
    return {m["id"]: m for m in r.json().get("data", [])}


def plan(models, routes, scenario_ids, modes):
    cells = []
    for sid in scenario_ids:
        sc = BY_ID[sid]
        for route in routes:
            if route not in sc.routes:
                continue
            for mode in modes:
                if mode not in sc.modes:
                    continue
                for model in ([UNKNOWN_MODEL] if sc.model_free else models):
                    cells.append((sc, mode, route, model))
    return cells


def _reasoning(card):
    return bool(((card or {}).get("capabilities") or {}).get("reasoning"))


def _compaction(card):
    """The card's on-demand compaction types (lmgw's capabilities.context_editing.compaction)."""
    editing = ((card or {}).get("capabilities") or {}).get("context_editing") or {}
    return "summarize" in (editing.get("compaction") or [])


def run_cell(sc, mode, route, model, cards, base, key, counter, timeout):
    res = {"key": f"{sc.id}|{mode}|{route}|{model}", "scenario": sc.id, "mode": mode, "route": route,
           "model": model, "status": "", "reason": "", "violations": [], "model_notes": [], "notes": [],
           "usage": {"in": 0, "out": 0}, "requests": 0, "seconds": 0.0}
    card = cards.get(model)
    if not sc.model_free and card is None:
        res.update(status=SKIPPED, reason=f"{model} is not on GET /v1/models")
        return res
    if sc.needs == "reasoning" and not _reasoning(card):
        res.update(status=SKIPPED, reason="the model card lists no reasoning control")
        return res
    if sc.needs == "compaction" and not _compaction(card):
        res.update(status=SKIPPED, reason="the model card lists no on-demand compaction (summarize)")
        return res
    temperature = None if sc.model_free or _reasoning(card) else 0
    kit = Kit(base, key, counter, timeout)
    kind = (((card or {}).get("capabilities") or {}).get("reasoning") or {}).get("kind", "")
    cell = Cell(ROUTES[route], model, mode == "stream", kit, temperature, [key], kind or "toggle")
    t0 = time.monotonic()
    status, reason = None, ""
    try:
        sc.fn(cell)
    except Outcome as o:
        status, reason = o.status, o.reason
    except Exception as e:  # noqa: BLE001 - a crash is reported, never hidden
        cell.violations.append(f"runner.crash: {type(e).__name__}: {e}")
        cell.notes.append(traceback.format_exc()[-1500:])
    finally:
        kit.close()
    if cell.violations:
        status, reason = FAIL, cell.violations[0]
    elif status is None:
        status = cell.status()
        reason = cell.model_notes[0] if cell.model_notes else ""
    res.update(status=status, reason=reason, violations=cell.violations, model_notes=cell.model_notes,
               notes=cell.notes, usage=cell.usage, requests=len(kit.rec.log),
               seconds=round(time.monotonic() - t0, 2))
    if status != "pass":
        res["evidence"] = cell.evidence()
    return res


def run(base, key, models, routes, scenario_ids, modes, jobs, timeout, progress):
    cards = fetch_cards(base, key, timeout)
    cells = plan(models, routes, scenario_ids, modes)
    counter = Counter()
    results = []
    with ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
        futs = [pool.submit(run_cell, *c, cards, base, key, counter, timeout) for c in cells]
        for n, f in enumerate(futs, 1):
            r = f.result()
            results.append(r)
            progress(n, len(futs), r)
    return results, cards, counter.n


def cost(results, cards):
    """Estimated USD from the usage the answers reported and /v1/models' per-token prices."""
    total, unpriced = 0.0, set()
    for r in results:
        if not r["usage"]["in"] and not r["usage"]["out"]:
            continue
        p = (cards.get(r["model"]) or {}).get("pricing") or {}
        try:
            total += r["usage"]["in"] * float(p["prompt"]) + r["usage"]["out"] * float(p["completion"])
        except (KeyError, TypeError, ValueError):
            unpriced.add(r["model"])
    return total, sorted(unpriced)


SCENARIO_IDS = [s.id for s in CATALOGUE]
