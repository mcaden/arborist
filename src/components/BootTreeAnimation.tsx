// Boot splash animation using the actual Arborist logo SVG.
// The SVG has leaf paths pre-classified with class="bl bl-N" (N = 0..6)
// by scripts/process-logo-svg.mjs. CSS animations target those classes
// for a gentle rustle effect. Trunk and root paths stay static.
// Respects prefers-reduced-motion.

import logoSvg from '@/assets/arborist-logo.svg?raw';

export function BootTreeAnimation(): JSX.Element {
  return (
    <div
      className="boot-tree"
      aria-hidden="true"
      // The processed SVG already has class annotations on leaf paths;
      // CSS in index.css animates .bl-0 through .bl-6.
      dangerouslySetInnerHTML={{ __html: logoSvg }}
    />
  );
}
