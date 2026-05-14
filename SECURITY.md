# Security Policy

## Reporting vulnerabilities

Report suspected vulnerabilities privately through GitHub Security Advisories for this repository. Please include the affected Arborist version or
commit, your operating system, reproduction steps, and any impact you observed. Do not open a public issue for an undisclosed vulnerability.

## Security posture

Arborist is a local-first Tauri desktop app. The WebView loads bundled assets only in production; it does not need remote network fetches for normal
operation. The production CSP in `src-tauri/tauri.conf.json` allows local scripts, local fonts/assets, `data:` images for OS-extracted icons, inline
styles required by React/xterm rendering, and Tauri IPC only.

Frontend code reaches privileged operations through typed Tauri commands in `src/lib/tauri-bridge.ts`. The main window capability grants only the
app-defined commands Arborist uses and the core event listen/unlisten permissions needed for backend events. Broad filesystem, shell, store, and
dialog plugin permissions are not granted. Plugin crates may still be registered for planned extension surfaces, but the WebView cannot invoke their
commands without a narrow capability grant. The directory picker is a narrow Rust command (`dialog_pick_directory`) backed by `rfd`.

Arborist must not store credentials. Authentication remains delegated to the user's installed CLI tools and their existing credential stores.
