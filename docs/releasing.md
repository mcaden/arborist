# Releasing

Arborist releases are built by the manual `Release` GitHub Actions workflow from an existing tag on `main`. The workflow uploads artifacts to a draft
GitHub Release and generates GitHub build attestations.

## Release prerequisites

- The release commit is merged to `main`.
- The tag already exists on the repository and is reachable from `main`.
- Version numbers match in:
  - `package.json`
  - `src-tauri/tauri.conf.json`
  - `src-tauri/Cargo.toml`
  - `crates/arborist-types/Cargo.toml`

After changing the Rust crate version, run a Cargo command that updates `Cargo.lock` if needed.

## Metadata policy

- The public app identifier is `io.github.mcaden.arborist`. Changing it later changes OS app identity and app data/config paths, so do not change it
  after public installs without a migration plan.
- `package.json`, Rust package metadata, and Tauri bundle metadata describe Arborist as a cross-platform desktop app for managing AI coding-assistant
  sessions across Git worktrees.
- Package metadata names Aaron Moore as author and `Arborist contributors` as contributors. Rust crates list both in `authors` because Cargo does not
  have a contributors field.
- Frontend and Rust packages are not published independently today: `package.json` stays `"private": true`, and Rust crates stay `publish = false`.
- Public homepage metadata points to `https://arborist.tools`; repository metadata points to `https://github.com/mcaden/arborist`.
- Keep the GitHub repository description and topics aligned with the README. Current launch topics: `tauri`, `react`, `rust`, `typescript`,
  `git-worktree`, and `ai-tools`.

## Cut a release

1. Land the version bump through PR.
2. Update `CHANGELOG.md`.
3. Tag the merge commit:

   ```sh
   git checkout main
   git pull
   git tag v0.1.1
   git push origin v0.1.1
   ```

4. Trigger **Actions -> Release -> Run workflow** from `main` and provide the tag.
5. Review the draft release and artifacts.
6. Smoke-test installers on clean machines or VMs.
7. Publish the draft release.

CLI equivalent for the workflow dispatch:

```sh
gh workflow run release.yml -f tag=v0.1.1
```

## Artifacts

| OS      | Artifacts                           |
| ------- | ----------------------------------- |
| Windows | NSIS `.exe` and WiX `.msi`.         |
| macOS   | Universal `.dmg` and app archive.   |
| Linux   | x86_64 AppImage and Debian package. |

The exact filenames are produced by Tauri and include the version.

## Trust and signing

Release artifacts are not OS code-signed unless a future release note says otherwise. Users should expect first-run warnings from Windows SmartScreen
and macOS Gatekeeper.

Every published release artifact should have a GitHub build attestation:

```sh
gh attestation verify <downloaded-file> --repo mcaden/arborist
```

Attestations prove the artifact was produced by the repository workflow for the referenced source, but they do not replace OS code signing.

## Release smoke checklist

- Windows installer completes and the app launches.
- Windows SmartScreen warning is dismissible.
- macOS DMG opens; right-click Open works on first launch.
- Linux AppImage runs after `chmod +x`.
- Linux `.deb` installs and launches.
- First-boot workspace picker accepts a primary clone and rejects a linked worktree.
- Existing sessions restore after restart.
- Worktree creation and optional prep banner work.
- Claude/Copilot launch commands are not accidentally hardcoded to a maintainer machine path.

## Dry runs

Use a throwaway tag to validate workflow changes. Delete the draft release and tag afterward.

## Out of scope today

- Apple notarization.
- Authenticode signing.
- Auto-updates.
- Package-manager distribution.
- ARM Linux release artifacts.
