// Plugin-dispatched tool icon renderer.
//
// Priority:
// 1) backend-cached executable icon (`iconDataUri`) when available,
// 2) plugin-owned SVG icon component from the AI registry,
// 3) generic fallback glyph.

import type { Tool } from '@/types/arborist';
import { useRegistry } from '@/plugins';

interface ToolIconProps {
  tool: Tool;
  className?: string;
  /**
   * Cached `data:image/png;base64,…` URI for the official CLI binary's
   * icon, resolved by the backend at config-save / startup time. When
   * present, renders as `<img>` in place of the bundled SVG glyph
   * below — so users get the actual Claude / Copilot brand icon when
   * the binary is on PATH, and a generic fallback otherwise.
   */
  iconDataUri?: string;
}

export function ToolIcon({ tool, className, iconDataUri }: ToolIconProps): JSX.Element {
  const plugin = useRegistry().aiById(tool);
  if (iconDataUri) {
    return (
      <img
        src={iconDataUri}
        alt=""
        aria-hidden="true"
        // `object-contain` so non-square icons (e.g. monochrome
        // Copilot mark vs square Claude mark) don't stretch.
        className={className ? `${className} object-contain` : 'object-contain'}
        draggable={false}
      />
    );
  }
  if (plugin) {
    const Icon = plugin.Icon;
    return className ? <Icon className={className} /> : <Icon />;
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
