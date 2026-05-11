#!/usr/bin/env bash
# Unix shim for the rustc-wrapper. Logic lives in sccache-deps-only.mjs.
exec node "$(dirname "$0")/sccache-deps-only.mjs" "$@"
