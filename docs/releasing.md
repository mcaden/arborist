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

## Project website and GitHub Pages

The public website is published from `website/` by `.github/workflows/pages.yml` using GitHub Actions artifact deployment. The primary public URL is
`https://arborist.tools`; `https://mcaden.github.io/arborist/` is the fallback and diagnostic GitHub Pages URL.

Preview the committed static site locally without starting the Tauri app:

```sh
pnpm run website:dev
```

The preview serves `http://127.0.0.1:4173/` and also maps `http://127.0.0.1:4173/arborist/` to the same files so the GitHub Pages fallback path can be
checked locally. Use `pnpm run website:dev -- --port 4174` if the default port is busy.

Keep these values aligned when changing site or release metadata:

- `website/CNAME`
- GitHub Pages custom-domain settings and DNS for `arborist.tools`
- the GitHub repository homepage URL
- `package.json` homepage metadata
- Rust package `homepage` fields
- Tauri bundle `homepage`

Pages uses **GitHub Actions** as the source with the `arborist.tools` custom domain. Maintainers keep DNS, HTTPS provisioning, and repository settings
aligned with `website/CNAME`. The website is curated for public users and contributors; it should not imply package-manager distribution or automatic
updates because signed installers are distributed through GitHub Releases.

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

Release artifacts are OS-signed and notarized where each platform supports it. Signed installers are distributed through GitHub Releases.

Every published release artifact has a GitHub build attestation:

```sh
gh attestation verify <downloaded-file> --repo mcaden/arborist
```

Attestations prove the artifact was produced by the repository workflow for the referenced source, but they do not replace OS code signing.

## Release smoke checklist

- Windows installer completes and the app launches.
- Windows installer publisher and signature are verified.
- macOS DMG opens and Gatekeeper verifies the notarized app.
- Linux AppImage runs after `chmod +x`.
- Linux `.deb` installs and launches.
- First-boot workspace picker accepts a primary clone and rejects a linked worktree.
- Existing sessions restore after restart.
- Worktree creation and optional prep banner work.
- Claude/Copilot launch commands are not accidentally hardcoded to a maintainer machine path.

## Dry runs

Use a throwaway tag to validate workflow changes. Delete the draft release and tag afterward.

## Out of scope today

- Auto-updates.
- Package-manager distribution.
- ARM Linux release artifacts.
