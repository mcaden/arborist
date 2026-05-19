// Boot splash animation using the actual Arborist logo SVG.
// The SVG has leaf paths pre-classified with class="bl bl-N" (N = 0..6)
// by scripts/process-logo-svg.mjs, and animation keyframes embedded in
// an inline <style> block. Rendering via <img> keeps the SVG sandboxed
// (no dangerouslySetInnerHTML) while preserving CSS animations.
// Respects prefers-reduced-motion via @media inside the SVG.

import logoSvgUrl from '@/assets/arborist-logo.svg';

export function BootTreeAnimation(): JSX.Element {
  return <img className="boot-tree" src={logoSvgUrl} alt="" aria-hidden="true" />;
}
