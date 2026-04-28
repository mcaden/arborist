// MIRROR: src-tauri/src/compose.rs::validate_worktree_name
//
// Pure validation for user-supplied worktree / branch names. Mirrors the
// rules enforced server-side so the new-session dialog can show inline
// feedback without round-tripping. The backend re-validates in
// `worktree_create_impl`; this is defence in depth.
//
// Returns `null` for a valid name, or a short human-readable error string
// otherwise.
export function validateWorktreeName(name: string): string | null {
  if (name.length === 0) return 'name cannot be empty';
  // Use Unicode-scalar count (Array.from) so this matches Rust's
  // chars().count() exactly across the boundary.
  if (Array.from(name).length > 255) return 'name cannot exceed 255 characters';
  if (name === '@') return "name cannot be '@'";
  if (name.startsWith('-')) return "name cannot start with '-'";
  if (name.includes('..')) return "name cannot contain '..'";
  if (name.includes('@{')) return "name cannot contain '@{'";
  if (name.includes('//')) return "name cannot contain '//'";
  if (name.includes(' ')) return 'name cannot contain spaces';
  for (const ch of ['~', '^', ':', '?', '*', '[', '\\']) {
    if (name.includes(ch)) return `name cannot contain '${ch}'`;
  }
  // ASCII control chars (\x00-\x1f) and DEL (\x7f).
  // eslint-disable-next-line no-control-regex
  if (/[\x00-\x1f\x7f]/.test(name)) return 'name cannot contain control characters';
  if (name.startsWith('.') || name.startsWith('/')) return "name cannot start with '.' or '/'";
  if (name.endsWith('.') || name.endsWith('/')) return "name cannot end with '.' or '/'";
  if (name.endsWith('.lock')) return "name cannot end with '.lock'";
  for (const component of name.split('/')) {
    if (component.length === 0) return 'name cannot contain empty path components';
    if (component.startsWith('.')) return "name path components cannot start with '.'";
    if (component.endsWith('.lock')) return "name path components cannot end with '.lock'";
  }
  return null;
}
