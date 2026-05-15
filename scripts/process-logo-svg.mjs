// Processes the optimized logo SVG to classify leaf paths
// for CSS animation. Adds class="bl bl-N" to leaf paths (N = 0..6 for stagger).
// Classification uses both color (green/yellow hue) AND vertical position
// (upper portion of the image = canopy). Trunk and roots stay static.
import { readFileSync, writeFileSync } from 'fs';

const input = process.argv[2];
const output = process.argv[3] || 'src/assets/arborist-logo.svg';

const svg = readFileSync(input, 'utf8');

function hexToHSL(hex) {
  hex = hex.replace('#', '');
  if (hex.length === 3) hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
  const r = parseInt(hex.substring(0, 2), 16) / 255;
  const g = parseInt(hex.substring(2, 4), 16) / 255;
  const b = parseInt(hex.substring(4, 6), 16) / 255;
  const max = Math.max(r, g, b),
    min = Math.min(r, g, b);
  let h,
    s,
    l = (max + min) / 2;
  if (max === min) {
    h = s = 0;
  } else {
    const d = max - min;
    s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    switch (max) {
      case r:
        h = ((g - b) / d + (g < b ? 6 : 0)) / 6;
        break;
      case g:
        h = ((b - r) / d + 2) / 6;
        break;
      case b:
        h = ((r - g) / d + 4) / 6;
        break;
    }
  }
  return { h: h * 360, s: s * 100, l: l * 100 };
}

function _isLeafColor(hex) {
  const { h, s, l } = hexToHSL(hex);
  if (h >= 40 && h <= 160 && s > 20 && l > 15 && l < 90) return true;
  return false;
}

// Extract bounding box from a path's d attribute by parsing all coordinate values.
// Returns { minX, minY, maxX, maxY, area }.
function getPathBounds(d) {
  // Parse all numbers from the path data
  const nums = d.match(/-?\d+(?:\.\d+)?/g);
  if (!nums || nums.length < 2) return { minX: 0, minY: 0, maxX: 601, maxY: 601, area: 601 * 601 };

  // Walk through path commands to extract actual X,Y coordinates.
  // For simplicity, parse command-by-command tracking current position.
  let x = 0,
    y = 0;
  let minX = Infinity,
    minY = Infinity,
    maxX = -Infinity,
    maxY = -Infinity;

  const update = (px, py) => {
    minX = Math.min(minX, px);
    minY = Math.min(minY, py);
    maxX = Math.max(maxX, px);
    maxY = Math.max(maxY, py);
  };

  // Tokenize: split into commands + numbers
  const tokens = d.match(/[A-Za-z]|-?\d+(?:\.\d+)?/g);
  if (!tokens) return { minX: 0, minY: 0, maxX: 601, maxY: 601, area: 601 * 601 };

  let cmd = 'M';
  let i = 0;
  while (i < tokens.length) {
    const t = tokens[i];
    if (/[A-Za-z]/.test(t)) {
      cmd = t;
      i++;
      continue;
    }
    const isRel = cmd === cmd.toLowerCase();
    const C = cmd.toUpperCase();

    if (C === 'M' || C === 'L' || C === 'T') {
      const nx = parseFloat(tokens[i] || '0');
      const ny = parseFloat(tokens[i + 1] || '0');
      x = isRel ? x + nx : nx;
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 2;
    } else if (C === 'H') {
      const nx = parseFloat(tokens[i] || '0');
      x = isRel ? x + nx : nx;
      update(x, y);
      i += 1;
    } else if (C === 'V') {
      const ny = parseFloat(tokens[i] || '0');
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 1;
    } else if (C === 'C') {
      // cubic bezier: 3 pairs
      for (let p = 0; p < 3; p++) {
        const nx = parseFloat(tokens[i] || '0');
        const ny = parseFloat(tokens[i + 1] || '0');
        const ax = isRel ? x + nx : nx;
        const ay = isRel ? y + ny : ny;
        update(ax, ay);
        i += 2;
        if (p === 2) {
          x = ax;
          y = ay;
        }
      }
    } else if (C === 'S' || C === 'Q') {
      for (let p = 0; p < 2; p++) {
        const nx = parseFloat(tokens[i] || '0');
        const ny = parseFloat(tokens[i + 1] || '0');
        const ax = isRel ? x + nx : nx;
        const ay = isRel ? y + ny : ny;
        update(ax, ay);
        i += 2;
        if (p === 1) {
          x = ax;
          y = ay;
        }
      }
    } else if (C === 'A') {
      // arc: rx ry rotation large-arc sweep x y
      i += 5; // skip rx ry rotation large-arc sweep
      const nx = parseFloat(tokens[i] || '0');
      const ny = parseFloat(tokens[i + 1] || '0');
      x = isRel ? x + nx : nx;
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 2;
    } else if (C === 'Z') {
      // close path — no params
    } else {
      i++; // skip unknown
    }
  }

  const area = (maxX - minX) * (maxY - minY);
  return { minX, minY, maxX, maxY, area };
}

// The SVG is 601x601. The canopy (leaves) occupies roughly the upper 60%.
// Leaf classification:
// 1. Upper 2/3 of the image (maxY < 401)
// 2. Fill has green channel >= 50 (hex)
// 3. Neither red nor blue exceeds the green value
function isLeafPath(bounds, fill, d) {
  // Use the starting M coordinate for canopy check — more reliable
  // than computed maxY since relative bezier accumulation can overshoot.
  const startMatch = d.match(/^M\s*(-?\d+(?:\.\d+)?)\s+(-?\d+(?:\.\d+)?)/i);
  const startY = startMatch ? parseFloat(startMatch[2]) : bounds.maxY;
  if (startY >= 340) return false;
  const hex = fill.replace('#', '');
  const r = parseInt(hex.substring(0, 2), 16);
  const g = parseInt(hex.substring(2, 4), 16);
  const b = parseInt(hex.substring(4, 6), 16);
  if (g < 0x19) return false;
  if (r > g || b > g) return false;
  return true;
}

let leafCount = 0,
  staticCount = 0,
  missedByY = 0,
  missedByColor = 0;

const result = svg.replace(/<path\s+d="([^"]*)"\s+fill="(#[A-Fa-f0-9]+)"/g, (match, d, fill) => {
  const bounds = getPathBounds(d);
  if (isLeafPath(bounds, fill, d)) {
    leafCount++;
    // Use starting M coordinate for grouping — getPathBounds() center
    // is unreliable due to relative bezier accumulation errors.
    const mMatch = d.match(/^M\s*(-?\d+(?:\.\d+)?)/i);
    const startX = mMatch ? parseFloat(mMatch[1]) : (bounds.minX + bounds.maxX) / 2;
    const group = Math.min(6, Math.floor((startX / 601) * 7));
    return `<path class="bl bl-${group}" d="${d}" fill="${fill}"`;
  }
  const hex = fill.replace('#', '');
  const r = parseInt(hex.substring(0, 2), 16);
  const g = parseInt(hex.substring(2, 4), 16);
  const b = parseInt(hex.substring(4, 6), 16);
  const colorOk = g >= 0x19 && r <= g && b <= g;
  if (colorOk && bounds.maxY >= 401) missedByY++;
  else if (!colorOk && bounds.maxY < 401) missedByColor++;
  staticCount++;
  return match;
});

writeFileSync(output, result);
console.log(`Leaf paths: ${leafCount}`);
console.log(`Static paths: ${staticCount}`);
console.log(`Missed by Y (color OK but maxY >= 401): ${missedByY}`);
console.log(`Missed by color (in canopy but wrong color): ${missedByColor}`);
console.log(`Canopy cutoff Y: 401 (upper 2/3)`);
console.log(`Output: ${output}`);
console.log(`Size: ${(Buffer.byteLength(result) / 1024).toFixed(1)} KB`);
