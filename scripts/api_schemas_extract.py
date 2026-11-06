"""Extracts the schemas lmgw's compatibility check needs from the providers' full specs.

Called by scripts/api-schemas-update.sh; reads the downloaded sources and
writes crates/lmgw-core/tests/fixtures/api_schemas/{openai,anthropic,gemini}.json
plus SOURCES.json, then prints what changed against the previous extraction.
Standard library only (python3 -I).

What it does to a source, and nothing else:

* keeps only the schemas reachable from the entry points below (the request
  bodies lmgw's egress sends, the answers and stream events its /v1 routes
  send), every `$ref` local (`#/$defs/<name>`);
* drops annotations no validator reads (description, title, examples,
  default, deprecated, `x-*`, OpenAPI's `discriminator`), so the pinned files
  stay small enough to review;
* writes OpenAI's `oneOf` unions as `anyOf`: the spec uses `oneOf` for a
  union whose alternatives overlap (an input message is both an
  `EasyInputMessage` and an `InputMessage`), which its server reads as
  "one of these kinds", not "exactly one schema matches";
* writes OpenAPI 3.0 leftovers as JSON Schema 2020-12 (`nullable: true` as a
  `null` alternative; the OpenAI spec is 3.1 and has none today);
* converts Gemini's discovery document (not JSON Schema) to JSON Schema:
  `$ref: Name` to `#/$defs/Name`, `type: any` to `{}`, a 64-bit integer
  (`format: int64`, sent as a JSON string or number in proto3 JSON) to
  string-or-integer, an enum to its names in upper or lower case (Google's
  JSON reads either, and Gemini's REST docs write `"thinkingLevel": "low"`);
* makes the **request** side strict: every object schema that lists
  `properties` and says nothing about `additionalProperties` gets
  `additionalProperties: false`, because all three providers refuse an
  unknown field (OpenAI "Unrecognized request argument", Anthropic "Extra
  inputs are not permitted", Gemini "Unknown name"). An `allOf` of objects is
  merged into one object first, so the members' properties are one set. The
  answer side stays as the provider wrote it: a client reading an answer
  ignores a field it does not know.

Usage: see scripts/api-schemas-update.sh.
"""

import argparse
import copy
import datetime
import hashlib
import json
import os
import sys

# Entry name -> where it is in the source. Request entries are strict (above).
OPENAI_REQUESTS = {
    "chat_request": "CreateChatCompletionRequest",
    "responses_request": "CreateResponse",
    "embeddings_request": "CreateEmbeddingRequest",
}
OPENAI_ANSWERS = {
    "chat_response": "CreateChatCompletionResponse",
    "chat_stream_chunk": "CreateChatCompletionStreamResponse",
    "response": "Response",
    "responses_stream_event": "ResponseStreamEvent",
    "embeddings_response": "CreateEmbeddingResponse",
    "models_response": "ListModelsResponse",
    "error_response": "ErrorResponse",
    "responses_input_tokens_response": "TokenCountsResource",
}
ANTHROPIC_REQUESTS = ["messages_request", "messages_request_streaming", "count_tokens_request"]
ANTHROPIC_ANSWERS = ["message", "stream_event", "count_tokens_response", "error_response",
                     "models_response"]
GEMINI_REQUESTS = {
    "generate_content_request": "GenerateContentRequest",
    "count_tokens_request": "CountTokensRequest",
    "batch_embed_contents_request": "BatchEmbedContentsRequest",
}

# Keywords that hold one subschema, a list of them, or a map of them.
ONE = ["items", "additionalProperties", "not", "if", "then", "else", "contains",
       "propertyNames", "unevaluatedProperties", "unevaluatedItems"]
LIST = ["anyOf", "oneOf", "allOf", "prefixItems"]
MAP = ["properties", "patternProperties", "$defs", "dependentSchemas"]
ANNOTATIONS = {"description", "title", "example", "examples", "default", "deprecated",
               "discriminator", "readOnly", "writeOnly", "$comment", "externalDocs"}


def walk(schema, fn):
    """Applies fn to every subschema bottom-up and returns the new schema."""
    if isinstance(schema, bool) or not isinstance(schema, dict):
        return schema
    out = {}
    for k, v in schema.items():
        if k in ONE and isinstance(v, (dict, bool)):
            out[k] = walk(v, fn)
        elif k in LIST and isinstance(v, list):
            out[k] = [walk(s, fn) for s in v]
        elif k in MAP and isinstance(v, dict):
            out[k] = {name: walk(s, fn) for name, s in v.items()}
        else:
            out[k] = v
    return fn(out)


def strip(schema):
    out = {k: v for k, v in schema.items() if k not in ANNOTATIONS and not k.startswith("x-")}
    if out.pop("nullable", False) is True:
        out = {"anyOf": [out, {"type": "null"}]}
    return out


def any_of(schema):
    """OpenAI's `oneOf` as `anyOf` (module doc)."""
    if "oneOf" in schema and "anyOf" not in schema:
        schema = dict(schema)
        schema["anyOf"] = schema.pop("oneOf")
    return schema


def refs_in(schema, found):
    if isinstance(schema, dict):
        r = schema.get("$ref")
        if isinstance(r, str) and r.startswith("#/$defs/"):
            found.add(r[len("#/$defs/"):])
        for v in schema.values():
            refs_in(v, found)
    elif isinstance(schema, list):
        for v in schema:
            refs_in(v, found)


def reachable(defs, roots):
    seen, todo = set(), list(roots)
    while todo:
        name = todo.pop()
        if name in seen:
            continue
        if name not in defs:
            sys.exit(f"api_schemas_extract: $ref to a missing schema {name}")
        seen.add(name)
        found = set()
        refs_in(defs[name], found)
        todo.extend(found - seen)
    return {n: defs[n] for n in sorted(seen)}


def strict(defs):
    """The request side's rule (module doc): allOf merged, unknown keys refused."""

    def resolve(s):
        while isinstance(s, dict) and set(s) == {"$ref"}:
            s = defs[s["$ref"][len("#/$defs/"):]]
        return s

    def merge_all_of(s):
        members = s.get("allOf")
        if not isinstance(members, list):
            return s
        # Each member closed as a schema of its own first (its nested objects
        # too); a closed one carries `additionalProperties: false`.
        resolved = [walk(copy.deepcopy(resolve(m)), close) for m in members]
        plain = {"type", "properties", "required", "additionalProperties"}
        if not all(isinstance(m, dict) and set(m) <= plain
                   and m.get("additionalProperties", False) is False for m in resolved):
            return s  # not plain objects: left as written, and not made strict
        merged = {k: v for k, v in s.items() if k != "allOf"}
        merged["type"] = "object"
        props = dict(merged.get("properties", {}))
        req = list(merged.get("required", []))
        for m in resolved:
            props.update(m.get("properties", {}))
            req += [r for r in m.get("required", []) if r not in req]
        merged["properties"] = props
        if req:
            merged["required"] = req
        return merged

    def close(s):
        s = merge_all_of(s)
        if "properties" in s and "additionalProperties" not in s \
                and "patternProperties" not in s and "allOf" not in s:
            s["additionalProperties"] = False
        return s

    return {n: walk(s, close) for n, s in defs.items()}


def bundle(defs, entries, strict_side):
    roots = set()
    for e in entries.values():
        refs_in(e, roots)
    kept = reachable(defs, roots)
    if strict_side:
        kept = strict(kept)
        kept = reachable(kept, roots)  # a merged allOf may orphan a member
    return {"entries": entries, "$defs": kept}


def openai(path):
    doc = json.load(open(path))
    if not doc.get("openapi", "").startswith("3.1"):
        sys.exit(f"api_schemas_extract: the OpenAI spec is OpenAPI {doc.get('openapi')}, "
                 "this extraction reads 3.1 (JSON Schema 2020-12)")
    raw = json.dumps(doc["components"]["schemas"]).replace("#/components/schemas/", "#/$defs/")
    defs = {n: walk(s, lambda x: any_of(strip(x))) for n, s in json.loads(raw).items()}
    ref = lambda n: {"$ref": f"#/$defs/{n}"}
    return {
        "request": bundle(defs, {k: ref(v) for k, v in OPENAI_REQUESTS.items()}, True),
        "answer": bundle(defs, {k: ref(v) for k, v in OPENAI_ANSWERS.items()}, False),
    }


def anthropic(path):
    dump = json.load(open(path))
    defs, entries = {}, {}
    for name, schema in dump["entries"].items():
        schema = walk(schema, strip)
        for d, s in schema.pop("$defs", {}).items():
            if d in defs and defs[d] != s:
                sys.exit(f"api_schemas_extract: two Anthropic schemas named {d} differ")
            defs[d] = s
        # Every entry becomes a $def of its own, so the entries are $refs too.
        key = f"entry_{name}"
        defs[key] = schema
        entries[name] = {"$ref": f"#/$defs/{key}"}
    pick = lambda names: {n: entries[n] for n in names}
    return {
        "request": bundle(defs, pick(ANTHROPIC_REQUESTS), True),
        "answer": bundle(defs, pick(ANTHROPIC_ANSWERS), False),
    }, dump["sdk"]


def gemini_schema(s):
    """One discovery-document schema as JSON Schema."""
    if "$ref" in s:
        return {"$ref": f"#/$defs/{s['$ref']}"}
    t = s.get("type")
    out = {}
    if t == "any":
        return {}
    if t == "string" and s.get("format") in ("int64", "uint64"):
        out["type"] = ["string", "integer"]
    elif t:
        out["type"] = t
    if "enum" in s:
        # Google's JSON reads an enum value in either case, and Gemini's own
        # REST docs write the lower-case spelling ("thinkingLevel": "low",
        # a function's "type": "object").
        out["enum"] = s["enum"] + [v.lower() for v in s["enum"] if v.lower() != v]
    if "properties" in s:
        out["properties"] = {k: gemini_schema(v) for k, v in s["properties"].items()}
    if "items" in s:
        out["items"] = gemini_schema(s["items"])
    if "additionalProperties" in s:
        out["additionalProperties"] = gemini_schema(s["additionalProperties"])
    return out


def gemini(path):
    doc = json.load(open(path))
    defs = {n: gemini_schema(s) for n, s in doc["schemas"].items()}
    entries = {k: {"$ref": f"#/$defs/{v}"} for k, v in GEMINI_REQUESTS.items()}
    return {"request": bundle(defs, entries, True)}, doc.get("revision")


def sha256(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def canonical_sha256(path):
    """Of the source's JSON with sorted keys: Google serves the same discovery
    document with its keys in a different order from one fetch to the next."""
    text = json.dumps(json.load(open(path)), sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(text.encode()).hexdigest()


def describe_change(old, new):
    """Per schema: added, removed, changed (what a reviewer reads first)."""
    lines = []
    for side in sorted(set(old) | set(new)):
        o = old.get(side, {}).get("$defs", {})
        n = new.get(side, {}).get("$defs", {})
        for name in sorted(set(o) | set(n)):
            if name not in o:
                lines.append(f"  {side}: + {name}")
            elif name not in n:
                lines.append(f"  {side}: - {name}")
            elif o[name] != n[name]:
                lines.append(f"  {side}: ~ {name}")
        if old.get(side, {}).get("entries") != new.get(side, {}).get("entries"):
            lines.append(f"  {side}: entry points changed")
    return lines


def layout(data):
    """One line per entry point and per schema, so a diff names what changed."""
    compact = lambda v: json.dumps(v, sort_keys=True, separators=(",", ":"))
    sides = []
    for side in sorted(data):
        b = data[side]
        lines = [f'  "entries": {{\n' + ",\n".join(
            f"   {json.dumps(k)}: {compact(v)}" for k, v in sorted(b["entries"].items())) + "\n  }"]
        lines.append('  "$defs": {\n' + ",\n".join(
            f"   {json.dumps(k)}: {compact(v)}" for k, v in sorted(b["$defs"].items())) + "\n  }")
        sides.append(f" {json.dumps(side)}: {{\n" + ",\n".join(lines) + "\n }")
    return "{\n" + ",\n".join(sides) + "\n}\n"


def write(out_dir, name, data):
    path = os.path.join(out_dir, name)
    old = json.load(open(path)) if os.path.exists(path) else {}
    text = layout(data)
    with open(path, "w") as f:
        f.write(text)
    changes = describe_change(old, data)
    print(f"{name}: {len(text.encode()) / 1024:.1f} KiB, "
          + (f"{len(changes)} change(s)" if changes else "unchanged"))
    for line in changes:
        print(line)


def main():
    p = argparse.ArgumentParser()
    for k in ["openai", "openai-url", "openai-commit", "anthropic", "anthropic-url",
              "anthropic-version", "pydantic-version", "anthropic-wheel-sha256", "anthropic-image", "pip-freeze", "gemini", "gemini-url", "out"]:
        p.add_argument(f"--{k}", required=True)
    a = p.parse_args()

    oa = openai(a.openai)
    an, sdk = anthropic(a.anthropic)
    if sdk["anthropic"] != a.anthropic_version:
        sys.exit(f"api_schemas_extract: the dump is SDK {sdk['anthropic']}, asked {a.anthropic_version}")
    ge, revision = gemini(a.gemini)

    today = datetime.date.today().isoformat()
    sources_path = os.path.join(a.out, "SOURCES.json")
    old_sources = json.load(open(sources_path)) if os.path.exists(sources_path) else {}
    digests = lambda path: {
        "sha256": sha256(path),
        "sha256_canonical": canonical_sha256(path),
        "bytes": os.path.getsize(path),
    }
    sources = {
        "openai": {
            "source": a.openai_url,
            "commit": a.openai_commit,
            **digests(a.openai),
        },
        "anthropic": {
            "source": a.anthropic_url,
            "how": "derived from the official Python SDK's request TypedDicts and response "
                   "models with pydantic's JSON Schema writer (scripts/api_schemas_anthropic_dump.py); "
                   "Anthropic publishes no standalone OpenAPI document",
            "sdk_version": a.anthropic_version,
            "pydantic_version": a.pydantic_version,
            "wheel_sha256": a.anthropic_wheel_sha256,
            # What the dump ran on: the container image (by digest) and every
            # package pip installed beside the SDK (`pip freeze`).
            "container_image": a.anthropic_image,
            "pip_freeze": [l.strip() for l in open(a.pip_freeze) if l.strip()],
            # The digests are of the derived dump (sorted keys, so a rerun on
            # the same SDK is byte-identical), not of the wheel.
            **digests(a.anthropic),
        },
        "gemini": {
            "source": a.gemini_url,
            "revision": revision,
            **digests(a.gemini),
        },
    }
    # The fetch date moves only when what was fetched did: its content, not
    # the order of its keys.
    for k, v in sources.items():
        prev = old_sources.get(k, {})
        same = prev.get("sha256_canonical") == v["sha256_canonical"]
        v["fetched"] = prev.get("fetched", today) if same else today
        if prev.get("sha256_canonical") and not same:
            print(f"SOURCES.json: {k} source changed "
                  f"({prev['sha256_canonical'][:12]} -> {v['sha256_canonical'][:12]})")
    with open(sources_path, "w") as f:
        f.write(json.dumps(sources, indent=1, sort_keys=True) + "\n")

    write(a.out, "openai.json", oa)
    write(a.out, "anthropic.json", an)
    write(a.out, "gemini.json", ge)


if __name__ == "__main__":
    main()
