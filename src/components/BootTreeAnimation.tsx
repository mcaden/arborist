// Animated SVG tree for the boot splash, inspired by the Arborist logo.
// Structured as grouped clusters for animation (not individual paths per leaf).
// Pure CSS @keyframes — no JS animation library. Respects prefers-reduced-motion.

export function BootTreeAnimation(): JSX.Element {
  return (
    <svg aria-hidden="true" focusable={false} viewBox="0 0 200 240" width="200" height="240" className="boot-tree">
      <defs>
        {/* Leaf gradient: light green → dark green */}
        <linearGradient id="leaf-grad" x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#8bc34a" />
          <stop offset="100%" stopColor="#2e7d32" />
        </linearGradient>
        {/* Trunk gradient: golden brown */}
        <linearGradient id="trunk-grad" x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#c68a2e" />
          <stop offset="60%" stopColor="#8b5e14" />
          <stop offset="100%" stopColor="#5d3a0a" />
        </linearGradient>
        {/* Root node glow */}
        <radialGradient id="node-glow">
          <stop offset="0%" stopColor="#ffd54f" />
          <stop offset="100%" stopColor="#c68a2e" />
        </radialGradient>
      </defs>

      {/* === TRUNK === */}
      <g className="boot-tree__trunk">
        {/* Main trunk */}
        <path d="M96 130 Q98 110 100 90 Q101 75 100 60 Q99 55 100 50" fill="none" stroke="url(#trunk-grad)" strokeWidth="8" strokeLinecap="round" />
        {/* Branch left-upper */}
        <path d="M100 65 Q90 55 75 48" fill="none" stroke="url(#trunk-grad)" strokeWidth="4" strokeLinecap="round" />
        {/* Branch right-upper */}
        <path d="M100 65 Q110 55 125 48" fill="none" stroke="url(#trunk-grad)" strokeWidth="4" strokeLinecap="round" />
        {/* Branch left-mid */}
        <path d="M99 80 Q85 72 65 65" fill="none" stroke="url(#trunk-grad)" strokeWidth="3.5" strokeLinecap="round" />
        {/* Branch right-mid */}
        <path d="M100 80 Q115 72 135 65" fill="none" stroke="url(#trunk-grad)" strokeWidth="3.5" strokeLinecap="round" />
        {/* Branch far-left */}
        <path d="M99 90 Q80 85 55 80" fill="none" stroke="url(#trunk-grad)" strokeWidth="3" strokeLinecap="round" />
        {/* Branch far-right */}
        <path d="M100 90 Q120 85 145 80" fill="none" stroke="url(#trunk-grad)" strokeWidth="3" strokeLinecap="round" />
      </g>

      {/* === CANOPY (leaf clusters) === */}
      <g className="boot-tree__canopy">
        {/* Top cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--top">
          <ellipse cx="100" cy="32" rx="14" ry="8" fill="url(#leaf-grad)" opacity="0.9" />
          <ellipse cx="90" cy="38" rx="10" ry="6" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="110" cy="38" rx="10" ry="6" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="100" cy="24" rx="9" ry="5" fill="#8bc34a" opacity="0.7" />
        </g>

        {/* Left upper cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--left-upper">
          <ellipse cx="72" cy="42" rx="12" ry="7" fill="url(#leaf-grad)" opacity="0.9" />
          <ellipse cx="62" cy="48" rx="10" ry="6" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="80" cy="36" rx="8" ry="5" fill="#7cb342" opacity="0.8" />
          <ellipse cx="55" cy="55" rx="9" ry="5" fill="url(#leaf-grad)" opacity="0.75" />
        </g>

        {/* Right upper cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--right-upper">
          <ellipse cx="128" cy="42" rx="12" ry="7" fill="url(#leaf-grad)" opacity="0.9" />
          <ellipse cx="138" cy="48" rx="10" ry="6" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="120" cy="36" rx="8" ry="5" fill="#7cb342" opacity="0.8" />
          <ellipse cx="145" cy="55" rx="9" ry="5" fill="url(#leaf-grad)" opacity="0.75" />
        </g>

        {/* Left mid cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--left-mid">
          <ellipse cx="58" cy="62" rx="11" ry="6" fill="url(#leaf-grad)" opacity="0.9" />
          <ellipse cx="48" cy="68" rx="9" ry="5" fill="url(#leaf-grad)" opacity="0.8" />
          <ellipse cx="68" cy="58" rx="8" ry="5" fill="#689f38" opacity="0.85" />
          <ellipse cx="42" cy="75" rx="8" ry="4.5" fill="url(#leaf-grad)" opacity="0.7" />
        </g>

        {/* Right mid cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--right-mid">
          <ellipse cx="142" cy="62" rx="11" ry="6" fill="url(#leaf-grad)" opacity="0.9" />
          <ellipse cx="152" cy="68" rx="9" ry="5" fill="url(#leaf-grad)" opacity="0.8" />
          <ellipse cx="132" cy="58" rx="8" ry="5" fill="#689f38" opacity="0.85" />
          <ellipse cx="158" cy="75" rx="8" ry="4.5" fill="url(#leaf-grad)" opacity="0.7" />
        </g>

        {/* Left lower cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--left-lower">
          <ellipse cx="50" cy="82" rx="10" ry="5.5" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="60" cy="78" rx="8" ry="5" fill="#558b2f" opacity="0.8" />
          <ellipse cx="43" cy="88" rx="7" ry="4" fill="url(#leaf-grad)" opacity="0.7" />
        </g>

        {/* Right lower cluster */}
        <g className="boot-tree__cluster boot-tree__cluster--right-lower">
          <ellipse cx="150" cy="82" rx="10" ry="5.5" fill="url(#leaf-grad)" opacity="0.85" />
          <ellipse cx="140" cy="78" rx="8" ry="5" fill="#558b2f" opacity="0.8" />
          <ellipse cx="157" cy="88" rx="7" ry="4" fill="url(#leaf-grad)" opacity="0.7" />
        </g>
      </g>

      {/* === GROUND LINE === */}
      <path d="M40 135 Q100 130 160 135" fill="none" stroke="#4a7c34" strokeWidth="2" opacity="0.5" />

      {/* === ROOTS (circuit-board style) === */}
      <g className="boot-tree__roots">
        {/* Main root down */}
        <path d="M98 132 L98 155 L90 170 L80 180" fill="none" stroke="url(#trunk-grad)" strokeWidth="3" strokeLinecap="round" />
        <path d="M100 132 L100 160 L110 175 L120 180" fill="none" stroke="url(#trunk-grad)" strokeWidth="3" strokeLinecap="round" />
        {/* Left branches */}
        <path d="M90 155 L75 160 L60 165" fill="none" stroke="url(#trunk-grad)" strokeWidth="2" strokeLinecap="round" />
        <path d="M80 170 L65 178 L50 185" fill="none" stroke="url(#trunk-grad)" strokeWidth="2" strokeLinecap="round" />
        <path d="M80 180 L70 192 L60 200" fill="none" stroke="url(#trunk-grad)" strokeWidth="1.5" strokeLinecap="round" />
        {/* Right branches */}
        <path d="M110 160 L125 165 L140 168" fill="none" stroke="url(#trunk-grad)" strokeWidth="2" strokeLinecap="round" />
        <path d="M120 175 L135 180 L150 185" fill="none" stroke="url(#trunk-grad)" strokeWidth="2" strokeLinecap="round" />
        <path d="M120 180 L130 192 L140 200" fill="none" stroke="url(#trunk-grad)" strokeWidth="1.5" strokeLinecap="round" />
        {/* Angular circuit junctions */}
        <path d="M75 160 L75 175" fill="none" stroke="url(#trunk-grad)" strokeWidth="1.5" strokeLinecap="round" />
        <path d="M125 165 L125 180" fill="none" stroke="url(#trunk-grad)" strokeWidth="1.5" strokeLinecap="round" />
      </g>

      {/* === ROOT TERMINAL NODES === */}
      <g className="boot-tree__nodes">
        <circle cx="60" cy="165" r="3.5" fill="url(#node-glow)" />
        <circle cx="50" cy="185" r="3.5" fill="url(#node-glow)" />
        <circle cx="60" cy="200" r="3" fill="url(#node-glow)" />
        <circle cx="75" cy="175" r="3" fill="url(#node-glow)" />
        <circle cx="80" cy="180" r="2.5" fill="url(#node-glow)" />
        <circle cx="140" cy="168" r="3.5" fill="url(#node-glow)" />
        <circle cx="150" cy="185" r="3.5" fill="url(#node-glow)" />
        <circle cx="140" cy="200" r="3" fill="url(#node-glow)" />
        <circle cx="125" cy="180" r="3" fill="url(#node-glow)" />
        <circle cx="120" cy="180" r="2.5" fill="url(#node-glow)" />
      </g>
    </svg>
  );
}
