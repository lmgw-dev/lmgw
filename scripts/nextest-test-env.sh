#!/usr/bin/env bash
# Usage: none by hand: cargo-nextest runs it as a setup script (.config/nextest.toml) before lmgw-core's it tests
#
# Writes two settings for those tests into $NEXTEST_ENV, both read by debug builds only:
#
# - LMGW_OPENAPI_DOC_CACHE names the file the tests share the admin API document through
#   (crates/lmgw-core/src/openapi/doc_cache.rs): the first test of the run that needs the document
#   builds it and writes it there, stamped with the run's id and the test binary, and every later
#   one reads it instead of building it again (seconds each). A file from another run is built
#   again, so it never outlives the build that wrote it; nothing is built or deleted here.
# - LMGW_TEST_ARGON2_FAST makes password hashes with argon2's smallest parameters
#   (crates/lmgw-core/src/accounts/password.rs); lmgw-core's unit tests keep RFC 9106's.
set -euo pipefail
: "${NEXTEST_ENV:?not run by cargo-nextest as a setup script}"
dir=$(realpath -m "${CARGO_TARGET_DIR:-target}/nextest")
mkdir -p "$dir"
{
  echo "LMGW_OPENAPI_DOC_CACHE=$dir/openapi-admin-doc.json"
  echo "LMGW_TEST_ARGON2_FAST=1"
} >>"$NEXTEST_ENV"
