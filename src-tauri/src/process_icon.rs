//! Best-effort application-icon extraction for `application`-kind
//! sub-sessions.
//!
//! ## Goal
//!
//! Replace the generic 🪟 emoji in the sidebar sub-tab with the actual
//! OS icon of the running application, so a user with three editors
//! and a file browser open can tell them apart at a glance.
//!
//! ## Trait seam + cache
//!
//! [`IconExtractor`] separates "find the exe path" from "extract a
//! PNG", so unit tests can substitute a [`FakeIconExtractor`] without
//! touching the OS. [`IconCache`] caches successful results by
//! **canonical exe path** rather than by PID:
//!
//! - PIDs recycle aggressively (especially on Linux) and would lead to stale
//!   icons if reused.
//! - Multiple sub-sessions running the same app share one cache entry —
//!   `Code.exe` open four times = one extraction.
//!
//! Failures are NOT cached: a transient miss (race during VS Code
//! retarget, brittle Linux `.desktop` lookup) shouldn't poison the
//! cache permanently. The frontend hook re-queries on each pid
//! transition anyway.
//!
//! ## Per-platform extractors (best-effort)
//!
//! - **Windows** — `QueryFullProcessImageNameW` → `SHGetFileInfoW` →
//!   `DrawIconEx` into a 32-bit BGRA DIB → swizzle to RGBA → PNG encode (`png`
//!   crate).
//! - **macOS** — `proc_pidpath` → walk up to `.app` bundle → `plutil -extract
//!   CFBundleIconFile raw …` → `sips -s format png` into a tempfile. No new
//!   Rust deps; relies on Apple's pre-installed binaries.
//! - **Linux** — read `/proc/<pid>/exe` → search XDG `applications/`
//!   directories for a matching `.desktop` file → resolve `Icon=`
//!   conservatively (absolute path, then a few standard
//!   `hicolor/<size>/apps/<name>.png` paths). No theme resolution; no SVG.
//!   Returns `None` on miss; emoji fallback in the UI is fine.
//!
//! ## What this module does NOT do
//!
//! - It does not invalidate cache entries. Exe paths don't recycle (VS Code
//!   self-update writes to a new versioned dir → new key). Cache size is
//!   bounded **at runtime** by [`MAX_CACHED_ICONS`] as a safety net against a
//!   runaway caller; the natural bound (distinct apps the user ever launches
//!   per session) is far smaller (~10).
//! - It does not return errors for the "no icon found" case, only `None`. The
//!   UI is supposed to fall back gracefully.
//! - It is NOT involved in the VS Code retarget flow itself; the hook in the
//!   frontend re-invokes [`Self::data_uri_for`] when the sub-session's pid
//!   changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;

/// Hard cap on the number of `(exe path → data URI)` entries the cache
/// holds. The realistic working set is tiny (5–20 distinct apps per
/// session), but a fuzz / malformed PID stream could otherwise grow
/// the map unboundedly. At 64 entries × ~50 KB per data URI that's
/// well under 4 MB — comfortably within steady-state. No eviction:
/// once the cap is reached we simply skip caching new exes (they're
/// still served live on each call).
const MAX_CACHED_ICONS: usize = 64;

/// Trait seam for testability. Production uses [`RealIconExtractor`].
pub trait IconExtractor: Send + Sync {
    /// Look up the executable path that backs `pid`. Returns `None`
    /// if the process has already exited or the platform doesn't
    /// support querying.
    fn exe_path(&self, pid: u32) -> Option<PathBuf>;

    /// Extract a PNG-encoded icon for `exe_path`. Returns `None` on
    /// any failure — callers (the frontend hook) treat that as
    /// "use the default emoji".
    fn extract_png(&self, exe_path: &Path) -> Option<Vec<u8>>;
}

/// Caching wrapper. Lookups are keyed by canonical exe path so
/// PID recycling can't serve a stale icon. See module docs for the
/// rationale.
pub struct IconCache {
    extractor: Arc<dyn IconExtractor>,
    by_exe: Mutex<HashMap<PathBuf, String>>,
}

impl IconCache {
    #[must_use]
    pub fn new(extractor: Arc<dyn IconExtractor>) -> Self {
        Self {
            extractor,
            by_exe: Mutex::new(HashMap::new()),
        }
    }

    /// Returns a `data:image/png;base64,…` URI for the running pid,
    /// or `None` if extraction fails. Caches the result by exe path
    /// on success; misses are re-attempted on subsequent calls.
    #[must_use]
    pub fn data_uri_for(&self, pid: u32) -> Option<String> {
        let exe = self.extractor.exe_path(pid)?;
        self.data_uri_for_path(&exe)
    }

    /// Variant that bypasses the PID → exe path lookup and queries
    /// the icon for an explicit executable path. Used when the
    /// caller has already resolved the executable from a command
    /// string (see [`crate::cmd_resolver`]).
    #[must_use]
    pub fn data_uri_for_path(&self, exe: &Path) -> Option<String> {
        // Canonicalise to dedupe `C:/Foo/bar.exe` vs `c:\foo\bar.exe`
        // on Windows; on Unix it resolves symlinks too. `dunce` is
        // already a workspace dep but isn't load-bearing here — a
        // simple `canonicalize` is enough since we only need
        // *stable* keys, not pretty-printable ones.
        let key = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
        if let Some(cached) = self.by_exe.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return Some(cached);
        }
        let png = self.extractor.extract_png(exe)?;
        let data_uri = format!("data:image/png;base64,{}", BASE64_STANDARD.encode(&png));
        if let Ok(mut m) = self.by_exe.lock() {
            // Refresh existing entries unconditionally; cap only
            // applies to *new* keys so a steady-state set keeps
            // working forever and a runaway caller stops growing
            // the map past `MAX_CACHED_ICONS`.
            if m.contains_key(&key) || m.len() < MAX_CACHED_ICONS {
                m.insert(key, data_uri.clone());
            } else {
                tracing::warn!(
                    cached = m.len(),
                    cap = MAX_CACHED_ICONS,
                    "icon cache at cap; serving live (not caching new entry)"
                );
            }
        }
        Some(data_uri)
    }

    #[cfg(test)]
    #[must_use]
    pub fn cached_count(&self) -> usize {
        self.by_exe.lock().map(|m| m.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Production extractor — delegates to platform module
// ---------------------------------------------------------------------------

/// Production [`IconExtractor`]. Delegates to the platform module
/// below; on unsupported platforms both methods return `None`.
pub struct RealIconExtractor;

impl IconExtractor for RealIconExtractor {
    fn exe_path(&self, pid: u32) -> Option<PathBuf> {
        platform::exe_path(pid)
    }
    fn extract_png(&self, exe_path: &Path) -> Option<Vec<u8>> {
        platform::extract_png(exe_path)
    }
}

// ---------------------------------------------------------------------------
// Platform: Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use std::ffi::c_void;
    use std::path::{Path, PathBuf};
    use std::ptr;

    #[allow(clippy::upper_case_acronyms)]
    type HDC = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type HBITMAP = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type HICON = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type HMODULE = *mut c_void;
    #[allow(clippy::upper_case_acronyms)]
    type HANDLE = *mut c_void;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type UINT = u32;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type WORD = u16;
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    type LONG = i32;

    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    const SHGFI_ICON: UINT = 0x0000_0100;
    const SHGFI_LARGEICON: UINT = 0x0000_0000;
    const DI_NORMAL: UINT = 0x0003;
    const BI_RGB: DWORD = 0;
    const DIB_RGB_COLORS: UINT = 0;

    #[repr(C)]
    struct ShFileInfoW {
        h_icon: HICON,
        i_icon: i32,
        dw_attributes: DWORD,
        sz_display_name: [u16; 260],
        sz_type_name: [u16; 80],
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: DWORD,
        bi_width: LONG,
        bi_height: LONG,
        bi_planes: WORD,
        bi_bit_count: WORD,
        bi_compression: DWORD,
        bi_size_image: DWORD,
        bi_x_pels_per_meter: LONG,
        bi_y_pels_per_meter: LONG,
        bi_clr_used: DWORD,
        bi_clr_important: DWORD,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        // No color table for 32-bit BI_RGB.
        _colors: [u32; 1],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(handle: HANDLE) -> BOOL;
        fn QueryFullProcessImageNameW(handle: HANDLE, flags: DWORD, buf: *mut u16, size: *mut DWORD) -> BOOL;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHGetFileInfoW(path: *const u16, file_attrs: DWORD, psfi: *mut ShFileInfoW, cb: UINT, flags: UINT) -> isize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn DestroyIcon(icon: HICON) -> BOOL;
        fn DrawIconEx(hdc: HDC, x: i32, y: i32, icon: HICON, cx: i32, cy: i32, frame_index: UINT, brush: HMODULE, flags: UINT) -> BOOL;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(hdc: HDC) -> HDC;
        fn DeleteDC(hdc: HDC) -> BOOL;
        fn CreateDIBSection(hdc: HDC, bmi: *const BitmapInfo, usage: UINT, bits: *mut *mut c_void, section: HANDLE, offset: DWORD) -> HBITMAP;
        fn DeleteObject(obj: HBITMAP) -> BOOL;
        fn SelectObject(hdc: HDC, obj: HBITMAP) -> HBITMAP;
    }

    pub(super) fn exe_path(pid: u32) -> Option<PathBuf> {
        // SAFETY: literal access mask + PID. Returns NULL handle on failure.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return None;
        }
        let mut buf: Vec<u16> = vec![0u16; 1024];
        let mut size: DWORD = buf.len() as DWORD;
        // SAFETY: buf has space for `size` u16s; size is &mut DWORD.
        let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
        // SAFETY: handle came from OpenProcess.
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        Some(PathBuf::from(path))
    }

    pub(super) fn extract_png(exe_path: &Path) -> Option<Vec<u8>> {
        let mut wpath: Vec<u16> = exe_path.as_os_str().encode_wide_collect();
        wpath.push(0);
        let mut sfi = ShFileInfoW {
            h_icon: ptr::null_mut(),
            i_icon: 0,
            dw_attributes: 0,
            sz_display_name: [0u16; 260],
            sz_type_name: [0u16; 80],
        };
        // SAFETY: wpath is null-terminated; sfi is a valid &mut.
        let res = unsafe {
            SHGetFileInfoW(
                wpath.as_ptr(),
                0,
                &mut sfi,
                std::mem::size_of::<ShFileInfoW>() as UINT,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if res == 0 || sfi.h_icon.is_null() {
            return None;
        }
        let icon = sfi.h_icon;
        let png = render_icon_to_png(icon, 32);
        // SAFETY: icon was returned by SHGetFileInfo; we're done with it.
        unsafe { DestroyIcon(icon) };
        png
    }

    /// Draw `hicon` into a 32-bit BGRA top-down DIB and PNG-encode
    /// the result. Size is fixed at 32×32 (the SHGFI_LARGEICON
    /// default). Returns `None` if any GDI call fails.
    fn render_icon_to_png(icon: HICON, size: i32) -> Option<Vec<u8>> {
        // SAFETY: literal NULL HDC argument is documented as
        // returning a screen-compatible DC.
        let hdc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if hdc.is_null() {
            return None;
        }
        let bmi = BitmapInfo {
            header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as DWORD,
                bi_width: size,
                // Negative height = top-down DIB (origin at top-left)
                // so we don't need to flip rows manually before PNG
                // encoding.
                bi_height: -size,
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: BI_RGB,
                bi_size_image: 0,
                bi_x_pels_per_meter: 0,
                bi_y_pels_per_meter: 0,
                bi_clr_used: 0,
                bi_clr_important: 0,
            },
            _colors: [0],
        };
        let mut bits: *mut c_void = ptr::null_mut();
        // SAFETY: hdc valid; bmi valid; bits is &mut.
        let dib = unsafe { CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0) };
        if dib.is_null() || bits.is_null() {
            // SAFETY: hdc was created successfully.
            unsafe { DeleteDC(hdc) };
            return None;
        }
        // SAFETY: hdc and dib are both valid GDI handles.
        let prev = unsafe { SelectObject(hdc, dib) };
        // SAFETY: hdc valid; icon valid; size literal.
        let drew = unsafe { DrawIconEx(hdc, 0, 0, icon, size, size, 0, ptr::null_mut(), DI_NORMAL) };
        // Restore the original bitmap before deleting the DIB.
        // SAFETY: prev was returned by SelectObject above.
        if !prev.is_null() {
            unsafe { SelectObject(hdc, prev) };
        }
        if drew == 0 {
            // SAFETY: dib + hdc valid.
            unsafe {
                DeleteObject(dib);
                DeleteDC(hdc);
            }
            return None;
        }
        let len = (size * size * 4) as usize;
        // SAFETY: CreateDIBSection guarantees `bits` points to at
        // least `bi_size_image` bytes (computed above). Top-down
        // 32bpp BI_RGB layout = 4 * width * height bytes BGRA.
        let bgra: &[u8] = unsafe { std::slice::from_raw_parts(bits as *const u8, len) };
        let mut rgba = bgra.to_vec();
        // BGRA → RGBA swizzle.
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        // Cleanup GDI before the (potentially fallible) PNG encode
        // so we don't leak handles on encode failure.
        // SAFETY: dib + hdc valid.
        unsafe {
            DeleteObject(dib);
            DeleteDC(hdc);
        }
        encode_rgba_png(&rgba, size as u32, size as u32)
    }

    fn encode_rgba_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
        let mut buf = Vec::with_capacity(rgba.len() / 2);
        {
            let mut encoder = png::Encoder::new(&mut buf, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(rgba).ok()?;
        }
        Some(buf)
    }

    /// Local extension trait so we don't need to add `windows-sys`
    /// just for `OsStrExt::encode_wide().collect()`.
    trait EncodeWideCollect {
        fn encode_wide_collect(&self) -> Vec<u16>;
    }

    impl EncodeWideCollect for std::ffi::OsStr {
        fn encode_wide_collect(&self) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            self.encode_wide().collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Platform: macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(super) fn exe_path(pid: u32) -> Option<PathBuf> {
        // `proc_pidpath` writes a NUL-terminated absolute path. The
        // canonical buffer size is 4 * MAXPATHLEN (4096 bytes ≈ enough
        // for any real macOS path).
        const PATH_MAX: usize = 4096;
        let mut buf = vec![0u8; PATH_MAX];
        // SAFETY: buf valid, length matches argument.
        let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), PATH_MAX as u32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        let s = String::from_utf8(buf).ok()?;
        Some(PathBuf::from(s))
    }

    pub(super) fn extract_png(exe_path: &Path) -> Option<Vec<u8>> {
        // Walk up to the first ancestor directory whose name ends
        // with `.app`. That's the bundle root.
        let bundle = find_app_bundle(exe_path)?;
        let plist = bundle.join("Contents").join("Info.plist");
        if !plist.exists() {
            return None;
        }
        // Ask plutil for the icon file name (preinstalled on macOS).
        let icon_name = run_capture("plutil", &["-extract", "CFBundleIconFile", "raw", plist.to_str()?])?
            .trim()
            .to_string();
        if icon_name.is_empty() {
            return None;
        }
        // The plist value may or may not have the .icns extension.
        let resources = bundle.join("Contents").join("Resources");
        let icns = {
            let with_ext = resources.join(format!("{icon_name}.icns"));
            if with_ext.exists() {
                with_ext
            } else {
                let plain = resources.join(&icon_name);
                if plain.exists() {
                    plain
                } else {
                    return None;
                }
            }
        };
        // Convert .icns → PNG via `sips` (preinstalled). Write to a
        // tempfile so we don't have to parse stdout.
        let tmp = tempfile::Builder::new().prefix("arborist-icon-").suffix(".png").tempfile().ok()?;
        // Pass paths as `OsStr` so we don't silently fail on bundles
        // whose names aren't valid UTF-8. `Command::arg` accepts
        // `AsRef<OsStr>`, so the OS path is forwarded verbatim.
        let status = Command::new("sips")
            .arg("-s")
            .arg("format")
            .arg("png")
            .arg(icns.as_os_str())
            .arg("--out")
            .arg(tmp.path().as_os_str())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        std::fs::read(tmp.path()).ok()
    }

    fn find_app_bundle(exe: &Path) -> Option<PathBuf> {
        let mut p = exe.parent();
        while let Some(dir) = p {
            if dir.file_name().and_then(|s| s.to_str()).map(|s| s.ends_with(".app")).unwrap_or(false) {
                return Some(dir.to_path_buf());
            }
            p = dir.parent();
        }
        None
    }

    fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }
}

// ---------------------------------------------------------------------------
// Platform: Linux
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::fs;
    use std::path::{Path, PathBuf};

    pub(super) fn exe_path(pid: u32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    pub(super) fn extract_png(exe_path: &Path) -> Option<Vec<u8>> {
        let exe_basename = exe_path.file_name()?.to_str()?.to_owned();
        let icon_name = find_icon_name_for_exe(&exe_basename)?;
        let candidate = Path::new(&icon_name);
        if candidate.is_absolute() {
            // Absolute `Icon=` path. The `.desktop` file is potentially
            // attacker-controlled (anyone who can write under
            // ~/.local/share/applications could otherwise make us read
            // /etc/shadow or block on /dev/zero). Require a real `.png`
            // file inside one of the standard XDG icon roots after
            // symlink resolution.
            if candidate.extension().and_then(|s| s.to_str()) != Some("png") {
                return None;
            }
            if !candidate.is_file() {
                return None;
            }
            if !is_within_allowed_root(candidate) {
                return None;
            }
            return fs::read(candidate).ok();
        }
        // Relative name. Reject path-traversal characters BEFORE feeding
        // to `Path::join`, which does not normalise `..` components and
        // would otherwise let `Icon=../../tmp/evil` escape the XDG roots.
        if !is_safe_relative_icon_name(&icon_name) {
            return None;
        }
        // Conservative XDG search. No theme resolution; PNG only.
        find_icon_in_xdg_paths(&icon_name)
    }

    /// True when `name` is a relative icon basename free of path
    /// separators and traversal components. Empty / `.` / `..` /
    /// anything containing `/` or `\` is rejected.
    fn is_safe_relative_icon_name(name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if name.contains('/') || name.contains('\\') {
            return false;
        }
        // `name` is a single path component at this point; reject the
        // self / parent specials defensively.
        !matches!(name, "." | "..")
    }

    /// True when `candidate` (assumed to exist) canonicalises inside one
    /// of the allowed XDG icon roots. Roots that don't exist on disk are
    /// silently skipped — this isn't a hard list, just the boundary we
    /// refuse to read outside of.
    fn is_within_allowed_root(candidate: &Path) -> bool {
        let Ok(canon_candidate) = candidate.canonicalize() else {
            return false;
        };
        for root in allowed_icon_roots() {
            let Ok(canon_root) = root.canonicalize() else {
                continue;
            };
            if canon_candidate.starts_with(&canon_root) {
                return true;
            }
        }
        false
    }

    /// Allow-list of XDG roots an absolute `Icon=` value may resolve
    /// inside. Mirrors `icon_search_bases()` plus the sibling `pixmaps`
    /// directories used by `find_icon_in_xdg_paths`.
    fn allowed_icon_roots() -> Vec<PathBuf> {
        let mut v = Vec::new();
        for icon_base in icon_search_bases() {
            if let Some(parent) = icon_base.parent() {
                v.push(parent.join("pixmaps"));
            }
            v.push(icon_base);
        }
        v
    }

    /// Return the `Icon=` value for a `.desktop` file whose `Exec=`
    /// starts with (after stripping `env … VAR=v` prefixes) the given
    /// executable basename. First match wins; users have many desktop
    /// files so we can't be exhaustive.
    fn find_icon_name_for_exe(exe_basename: &str) -> Option<String> {
        let dirs = applications_dirs();
        for dir in dirs {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                    continue;
                }
                let raw = match fs::read_to_string(&path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if !exec_matches_basename(&raw, exe_basename) {
                    continue;
                }
                if let Some(name) = read_key(&raw, "Icon") {
                    return Some(name);
                }
            }
        }
        None
    }

    fn applications_dirs() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(&home).join(".local/share/applications"));
        }
        v.push(PathBuf::from("/usr/share/applications"));
        v.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
        v.push(PathBuf::from("/usr/local/share/applications"));
        v
    }

    /// True if the desktop file's first `Exec=` line resolves to a
    /// command whose basename matches `exe_basename` (case-sensitive
    /// — Linux exe names are case-sensitive).
    fn exec_matches_basename(raw: &str, exe_basename: &str) -> bool {
        let Some(exec) = read_key(raw, "Exec") else {
            return false;
        };
        // Tokenise on whitespace; skip `env` and any `KEY=VAL` prefix
        // tokens until we hit the actual program.
        let mut tokens = exec.split_whitespace();
        while let Some(t) = tokens.next() {
            if t == "env" {
                continue;
            }
            if t.contains('=') && !t.starts_with('/') && !t.contains('/') {
                // Looks like FOO=bar — keep skipping.
                continue;
            }
            // First real program token. Strip any `%U`/`%f` field
            // codes — those only appear in later tokens, not here.
            let prog = Path::new(t).file_name().and_then(|s| s.to_str());
            return prog.is_some_and(|p| p == exe_basename);
        }
        false
    }

    /// Read the first occurrence of `key=` from a `.desktop` file's
    /// `[Desktop Entry]` section. Conservative: doesn't track
    /// section boundaries strictly, but works for well-formed files
    /// where the Desktop Entry section is first.
    fn read_key(raw: &str, key: &str) -> Option<String> {
        let needle = format!("{key}=");
        for line in raw.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix(&needle) {
                return Some(rest.trim().to_owned());
            }
        }
        None
    }

    /// Try a handful of sensible PNG locations under standard XDG
    /// roots. We deliberately don't walk every theme size on every
    /// disk — that's the OS's job, not ours.
    fn find_icon_in_xdg_paths(name: &str) -> Option<Vec<u8>> {
        let bases = icon_search_bases();
        let sizes = ["256x256", "128x128", "64x64", "48x48", "32x32"];
        for base in &bases {
            for size in &sizes {
                let p = base.join("hicolor").join(size).join("apps").join(format!("{name}.png"));
                if p.is_file() {
                    if let Ok(b) = fs::read(&p) {
                        return Some(b);
                    }
                }
            }
            // Pixmaps fallback (no size dir).
            let p = base.parent().map(|p| p.join("pixmaps"));
            if let Some(pix) = p {
                let candidate = pix.join(format!("{name}.png"));
                if candidate.is_file() {
                    if let Ok(b) = fs::read(&candidate) {
                        return Some(b);
                    }
                }
            }
        }
        None
    }

    fn icon_search_bases() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(&home).join(".local/share/icons"));
        }
        v.push(PathBuf::from("/usr/share/icons"));
        v.push(PathBuf::from("/usr/local/share/icons"));
        v
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn exec_matches_basename_handles_simple_exec() {
            let desktop = "[Desktop Entry]\nName=Foo\nExec=foo --bar %U\nIcon=foo\n";
            assert!(exec_matches_basename(desktop, "foo"));
        }

        #[test]
        fn exec_matches_basename_handles_absolute_path() {
            let desktop = "[Desktop Entry]\nExec=/usr/bin/firefox %u\n";
            assert!(exec_matches_basename(desktop, "firefox"));
            assert!(!exec_matches_basename(desktop, "fire"));
        }

        #[test]
        fn exec_matches_basename_handles_env_prefix() {
            let desktop = "[Desktop Entry]\nExec=env MOZ_USE_XINPUT2=1 /usr/lib/firefox/firefox %u\n";
            assert!(exec_matches_basename(desktop, "firefox"));
        }

        #[test]
        fn exec_matches_basename_handles_kvar_prefix() {
            let desktop = "[Desktop Entry]\nExec=FOO=1 BAR=2 /opt/code/code %F\n";
            assert!(exec_matches_basename(desktop, "code"));
        }

        #[test]
        fn read_key_returns_first_match() {
            let raw = "[Desktop Entry]\nIcon=visual-studio-code\nName=Code\n";
            assert_eq!(read_key(raw, "Icon").as_deref(), Some("visual-studio-code"));
        }

        #[test]
        fn read_key_returns_none_when_missing() {
            let raw = "[Desktop Entry]\nName=Code\n";
            assert!(read_key(raw, "Icon").is_none());
        }

        #[test]
        fn is_safe_relative_icon_name_accepts_simple_names() {
            assert!(is_safe_relative_icon_name("firefox"));
            assert!(is_safe_relative_icon_name("visual-studio-code"));
            assert!(is_safe_relative_icon_name("app_icon"));
            assert!(is_safe_relative_icon_name("a.b"));
        }

        #[test]
        fn is_safe_relative_icon_name_rejects_path_traversal() {
            assert!(!is_safe_relative_icon_name(""));
            assert!(!is_safe_relative_icon_name("."));
            assert!(!is_safe_relative_icon_name(".."));
            assert!(!is_safe_relative_icon_name("../etc/passwd"));
            assert!(!is_safe_relative_icon_name("../../tmp/evil"));
            assert!(!is_safe_relative_icon_name("foo/bar"));
            assert!(!is_safe_relative_icon_name("foo\\bar"));
            assert!(!is_safe_relative_icon_name("/etc/passwd"));
        }

        #[test]
        fn is_within_allowed_root_rejects_outside_paths() {
            // /etc/shadow obviously isn't an icon root. The check
            // should reject it whether it exists or not — `canonicalize`
            // returns Err for paths the test process can't read, which
            // also resolves to false. Either way: rejected.
            assert!(!is_within_allowed_root(Path::new("/etc/shadow")));
            assert!(!is_within_allowed_root(Path::new("/dev/zero")));
            assert!(!is_within_allowed_root(Path::new("/tmp/arborist-test-not-an-icon-root")));
        }

        #[test]
        fn is_within_allowed_root_accepts_real_icon_root_when_present() {
            // Skip on hosts where no standard icon root exists (CI
            // containers, minimal images). We can only positively
            // assert acceptance when the OS actually has an XDG icon
            // tree to canonicalise against.
            let real_root = allowed_icon_roots().into_iter().find(|r| r.is_dir() && r.canonicalize().is_ok());
            let Some(root) = real_root else {
                eprintln!("skipping: no XDG icon root present on this host");
                return;
            };
            // The root itself must be considered "within" itself
            // (`starts_with(canon_root)` is reflexive on canonicalised
            // paths).
            assert!(is_within_allowed_root(&root));
        }
    }
}

// ---------------------------------------------------------------------------
// Platform: fallback (no-op)
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
mod platform {
    use std::path::{Path, PathBuf};

    pub(super) fn exe_path(_pid: u32) -> Option<PathBuf> {
        None
    }

    pub(super) fn extract_png(_exe_path: &Path) -> Option<Vec<u8>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests (cross-platform: cache logic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test extractor: returns the canned exe path / png; counts how
    /// many times each method was invoked so cache hits/misses are
    /// observable.
    struct FakeExtractor {
        exe: Option<PathBuf>,
        png: Option<Vec<u8>>,
        exe_calls: AtomicUsize,
        png_calls: AtomicUsize,
    }

    impl FakeExtractor {
        fn new(exe: Option<PathBuf>, png: Option<Vec<u8>>) -> Arc<Self> {
            Arc::new(Self {
                exe,
                png,
                exe_calls: AtomicUsize::new(0),
                png_calls: AtomicUsize::new(0),
            })
        }
    }

    impl IconExtractor for FakeExtractor {
        fn exe_path(&self, _pid: u32) -> Option<PathBuf> {
            self.exe_calls.fetch_add(1, Ordering::SeqCst);
            self.exe.clone()
        }
        fn extract_png(&self, _exe_path: &Path) -> Option<Vec<u8>> {
            self.png_calls.fetch_add(1, Ordering::SeqCst);
            self.png.clone()
        }
    }

    #[test]
    fn cache_returns_data_uri_on_success() {
        let fake = FakeExtractor::new(Some(PathBuf::from("foo.exe")), Some(vec![1, 2, 3]));
        let cache = IconCache::new(fake);
        let uri = cache.data_uri_for(123).expect("data uri");
        assert!(uri.starts_with("data:image/png;base64,"));
        // base64("\x01\x02\x03") = "AQID"
        assert!(uri.ends_with("AQID"), "unexpected encoding: {uri}");
    }

    #[test]
    fn cache_hit_does_not_re_extract() {
        let fake = FakeExtractor::new(Some(PathBuf::from("foo.exe")), Some(vec![9]));
        let cache = IconCache::new(fake.clone());
        let _ = cache.data_uri_for(1).unwrap();
        let _ = cache.data_uri_for(2).unwrap();
        assert_eq!(
            fake.png_calls.load(Ordering::SeqCst),
            1,
            "second lookup must hit the cache (same exe path) and not re-extract"
        );
        assert_eq!(cache.cached_count(), 1);
    }

    #[test]
    fn cache_miss_returns_none_without_caching() {
        let fake = FakeExtractor::new(Some(PathBuf::from("foo.exe")), None);
        let cache = IconCache::new(fake.clone());
        assert!(cache.data_uri_for(1).is_none());
        assert!(cache.data_uri_for(1).is_none());
        assert_eq!(
            fake.png_calls.load(Ordering::SeqCst),
            2,
            "miss must NOT be cached — re-attempt on next lookup"
        );
        assert_eq!(cache.cached_count(), 0);
    }

    #[test]
    fn unknown_pid_returns_none_without_extracting() {
        let fake = FakeExtractor::new(None, Some(vec![1]));
        let cache = IconCache::new(fake.clone());
        assert!(cache.data_uri_for(999).is_none());
        assert_eq!(fake.png_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn different_exes_get_separate_cache_entries() {
        // Same fake but exe_path returns different paths each call.
        struct AlternatingExtractor {
            counter: AtomicUsize,
        }
        impl IconExtractor for AlternatingExtractor {
            fn exe_path(&self, _pid: u32) -> Option<PathBuf> {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                Some(PathBuf::from(format!("app-{n}.exe")))
            }
            fn extract_png(&self, _exe_path: &Path) -> Option<Vec<u8>> {
                Some(vec![0u8])
            }
        }
        let cache = IconCache::new(Arc::new(AlternatingExtractor {
            counter: AtomicUsize::new(0),
        }));
        let _ = cache.data_uri_for(1).unwrap();
        let _ = cache.data_uri_for(2).unwrap();
        assert_eq!(cache.cached_count(), 2);
    }

    #[test]
    fn cache_caps_growth_at_max_cached_icons() {
        // Hammer a unique exe per call — without the cap, the map would
        // grow unboundedly. With the cap, it must stop at
        // MAX_CACHED_ICONS even though every call still returns a
        // valid live URI.
        struct UniqueExtractor {
            counter: AtomicUsize,
        }
        impl IconExtractor for UniqueExtractor {
            fn exe_path(&self, _pid: u32) -> Option<PathBuf> {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                Some(PathBuf::from(format!("/tmp/arborist-icon-cap-app-{n}.exe")))
            }
            fn extract_png(&self, _exe_path: &Path) -> Option<Vec<u8>> {
                Some(vec![0u8])
            }
        }
        let cache = IconCache::new(Arc::new(UniqueExtractor {
            counter: AtomicUsize::new(0),
        }));
        for pid in 0..(MAX_CACHED_ICONS as u32 + 8) {
            assert!(cache.data_uri_for(pid).is_some());
        }
        assert_eq!(cache.cached_count(), MAX_CACHED_ICONS, "cache must stop growing at the configured cap");
    }
}
