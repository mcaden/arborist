# Releasing Arborist

Arborist ships as unsigned, platform-native installers built by the
[`Release` GitHub Actions workflow](../.github/workflows/release.yml). The
workflow runs on every `v*` tag push and uploads its artifacts to a **draft**
GitHub Release for review before publication.

## One-time setup

None. The workflow uses the default `GITHUB_TOKEN`; no secrets to configure.

## Cutting a release

1. **Bump the version** in all three places (they must match):
   - `package.json` → `"version"`
   - `src-tauri/tauri.conf.json` → `"version"`
   - `src-tauri/Cargo.toml` → `[package] version`
     Then run `cargo update -p arborist --precise <new-version>` so `Cargo.lock`
     tracks it.
2. **Commit** the bump on `main` (via PR) — e.g. `chore: bump to v0.1.1`.
3. **Tag and push**:
   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```
4. **Wait for the workflow.** The `Release` workflow runs three jobs in
   parallel — Windows, macOS (universal), Linux (Ubuntu 22.04, x86_64) — and
   uploads bundles to a draft release named `Arborist v0.1.1`.
5. **Smoke-test the artifacts.** Download each installer and confirm the app
   launches on a clean machine (or VM). Pay attention to:
   - macOS Gatekeeper right-click → Open works on Apple Silicon and Intel.
   - Windows SmartScreen warning is dismissable and the installer completes.
   - The Linux AppImage runs after `chmod +x`; the `.deb` installs cleanly.
6. **Publish the draft** from the GitHub Releases UI.

## Manual / dry-run builds

Use the workflow's `workflow_dispatch` trigger with a throwaway tag like
`v0.0.1-test` to validate workflow changes without cutting a real release.
Delete the resulting draft release and the test tag afterward.

## Artifacts produced

| OS      | Files                                                                               |
| ------- | ----------------------------------------------------------------------------------- |
| Windows | `Arborist_<version>_x64-setup.exe` (NSIS), `Arborist_<version>_x64_en-US.msi` (WiX) |
| macOS   | `Arborist_<version>_universal.dmg`, `Arborist_<version>_universal.app.tar.gz`       |
| Linux   | `arborist_<version>_amd64.AppImage`, `arborist_<version>_amd64.deb`                 |

Bundle target selection is driven by `bundle.targets: "all"` in
`tauri.conf.json` — each runner produces only the bundle types its OS
supports.

## Out of scope

Code signing, notarization, auto-updates, ARM Linux builds, and distribution
to package managers (Homebrew, winget, Chocolatey, Flathub, AUR) are not
configured. See `dev/ai/installer.md` for the rationale.
