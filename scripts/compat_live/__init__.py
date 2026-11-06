"""Live API compatibility check of a running lmgw (docs/design/2026-11-04-api-compat-design.md).

Run it through scripts/compat-live.sh, which starts it in a rootless Podman container.
The modules split by concern: sse, common, classify and rules_* are pure (no third-party
imports; scripts/compat_live_test.py tests them), route_* build and read each protocol's
bodies, scenarios is the catalogue, transport and sdk_check talk to lmgw through the
official SDKs, runner walks the matrix and report writes results.json, report.html and
the diff against the newest earlier run that covers this one (or, when none does, the one
sharing the most cells with it, named as a partial baseline).
"""
