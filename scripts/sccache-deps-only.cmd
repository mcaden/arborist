@echo off
rem Windows shim for the rustc-wrapper. Logic lives in sccache-deps-only.mjs.
node "%~dp0sccache-deps-only.mjs" %*
