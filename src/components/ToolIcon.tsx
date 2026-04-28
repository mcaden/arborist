// Inline SVG icons for the Sidebar. The same glyphs live as standalone files
// under `src/assets/{claude,copilot}-icon.svg` for reference; we inline them
// here so Tailwind `text-*` classes can recolour them via `currentColor`
// (browsers don't inherit colour into `<img>` sources).
//
// NOTE: these are placeholder marks, not the official Claude / Copilot logos.

import type { Tool } from '@/types/grove';

interface ToolIconProps {
  tool: Tool;
  className?: string;
}

export function ToolIcon({ tool, className }: ToolIconProps): JSX.Element {
  if (tool === 'claude') {
    return (
      <svg
        xmlns="http://www.w3.org/2000/svg"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth={2}
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
        className={className}
      >
        <circle cx="12" cy="12" r="9" />
        <path d="M15.5 8.5 A5 5 0 1 0 15.5 15.5" />
      </svg>
    );
  }
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={className}
    >
      <path d="M4 13c0-2 1-3 2.5-3.5C7 6 9 4 12 4s5 2 5.5 5.5C19 10 20 11 20 13v3c0 2-3 4-8 4s-8-2-8-4z" />
      <circle cx="9.5" cy="14" r="1.25" fill="currentColor" stroke="none" />
      <circle cx="14.5" cy="14" r="1.25" fill="currentColor" stroke="none" />
    </svg>
  );
}
