# Releasing

Arborist releases are built by the manual `Release` GitHub Actions workflow from an existing tag on `main`. The workflow uploads artifacts to a draft
GitHub Release and generates GitHub build attestations.

For branch-level validation (without publishing), use the manual `Release Verify Builds` workflow. It can be run from any branch and verifies macOS,
Windows, and Linux installer artifacts are produced.

## Version convention

The version in manifests on `main` is always the **upcoming** release version. When you're ready to release, the `release:prep` script tags the current
HEAD (closing out that version), then branches to bump manifests to the next development version and opens a PR.

## Release prerequisites

- All manifests on `main` agree on the version to be released:
  - `package.json`
  - `src-tauri/tauri.conf.json`
  - `src-tauri/Cargo.toml`
  - `crates/arborist-types/Cargo.toml`
- The working tree is clean and you're on `main`.

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

```sh
git checkout main
git pull
pnpm run release:prep 0.1.3
```

If the script fails _after_ the tag was pushed (e.g. network blip during the bump PR), replay only the bump:

```sh
pnpm run release:prep --skip-tag 0.1.3
```

The script:

1. Reads the current version from manifests (e.g. `0.1.2`) — this is the version being released.
2. Validates: on `main`, clean tree, tag `v0.1.2` doesn't exist, all 4 manifests agree.
3. Tags current HEAD as `v0.1.2` and pushes the tag (no commits to `main`).
4. Creates branch `chore/bump-0.1.3`, bumps all manifests to `0.1.3`, commits, pushes, opens a PR.
5. Returns to `main`.

After the script completes, trigger the release build:

```sh
gh workflow run release.yml -f tag=v0.1.2
```

Then merge the version-bump PR once CI passes.

<details>
<summary>Manual steps (if not using release:prep)</summary>

1. Ensure the version in manifests on `main` is the version you want to release (the repo convention keeps the _upcoming_ version in code).
2. Tag the HEAD commit:

   ```sh
   git checkout main
   git pull
   git tag -a v0.1.2 -m "Release v0.1.2"
   git push origin v0.1.2
   ```

3. Trigger **Actions -> Release -> Run workflow** from `main` and provide the tag.
4. Create a branch from `main`, bump all 4 manifests to the next version (e.g. `0.1.3`), run `cargo update -p arborist -p arborist-types`, commit, push,
   and open a PR targeting `main`.

</details>

## After the workflow runs

1. Review the draft release and artifacts.
2. Smoke-test installers on clean machines or VMs.
3. Publish the draft release.

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

Use the manual `Release Verify Builds` workflow to validate release build changes without creating a draft release.

```sh
gh workflow run release-verify-builds.yml
```

Use a throwaway tag with `Release` only when you specifically need to exercise draft release creation or attestation/upload behavior.

Run a dry release whenever release workflow action pins change, especially for Tauri publishing or build-attestation actions. The workflow pins every
`uses:` dependency to a full commit SHA with an inline comment naming the upstream action ref; Dependabot opens update PRs, but maintainers should verify
the new SHA and comment before merging.

## Out of scope today

- Auto-updates.
- Package-manager distribution.
- ARM Linux release artifacts.
