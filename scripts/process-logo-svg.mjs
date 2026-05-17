// Processes the optimized logo SVG to classify leaf paths
// for CSS animation. Adds class="bl bl-N" to leaf paths (N = 0..6 for stagger).
// Classification uses both color (green/yellow hue) AND vertical position
// (upper portion of the image = canopy). Trunk and roots stay static.
import { readFileSync, writeFileSync } from 'fs';

const input = process.argv[2];
const output = process.argv[3] || 'src/assets/arborist-logo.svg';

const svg = readFileSync(input, 'utf8');

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

// Normalize a fill value to 6-digit hex. Returns null for named colors or unsupported formats.
function normalizeFill(fill) {
  if (!fill.startsWith('#')) return null;
  let hex = fill.slice(1);
  if (hex.length === 3) hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
  if (hex.length !== 6 || !/^[0-9a-fA-F]{6}$/.test(hex)) return null;
  return '#' + hex;
}

// The SVG is 601x601. The canopy (leaves) occupies roughly the upper 60%.
// Leaf classification:
// 1. Upper 2/3 of the image (startY < 340)
// 2. Fill has green channel >= 25 (0x19)
// 3. Neither red nor blue exceeds the green value
function isLeafPath(bounds, hexFill, d) {
  // Use the starting M coordinate for canopy check — more reliable
  // than computed maxY since relative bezier accumulation can overshoot.
  const startMatch = d.match(/^M\s*(-?\d+(?:\.\d+)?)\s+(-?\d+(?:\.\d+)?)/i);
  const startY = startMatch ? parseFloat(startMatch[2]) : bounds.maxY;
  if (startY >= 340) return false;
  const hex = hexFill.replace('#', '');
  const r = parseInt(hex.substring(0, 2), 16);
  const g = parseInt(hex.substring(2, 4), 16);
  const b = parseInt(hex.substring(4, 6), 16);
  if (g < 0x19) return false;
  if (r > g || b > g) return false;
  return true;
}

let leafCount = 0,
  staticCount = 0,
  strippedCount = 0;

// Match paths with any fill attribute (hex or named color)
const result = svg.replace(/<path\s+d="([^"]*)"\s+fill="([^"]+)"/g, (match, d, fill) => {
  const normalized = normalizeFill(fill);
  if (!normalized) {
    // Named color or unsupported format — strip (replace fill with transparent)
    strippedCount++;
    return `<path d="${d}" fill="none"`;
  }
  const bounds = getPathBounds(d);
  if (isLeafPath(bounds, normalized, d)) {
    leafCount++;
    const mMatch = d.match(/^M\s*(-?\d+(?:\.\d+)?)/i);
    const startX = mMatch ? parseFloat(mMatch[1]) : (bounds.minX + bounds.maxX) / 2;
    const group = Math.min(6, Math.floor((startX / 601) * 7));
    return `<path class="bl bl-${group}" d="${d}" fill="${normalized}"`;
  }
  staticCount++;
  // Always emit normalized fill (expands 3-digit hex to 6-digit)
  return `<path d="${d}" fill="${normalized}"`;
});

writeFileSync(output, result);
console.log(`Leaf paths: ${leafCount}`);
console.log(`Static paths: ${staticCount}`);
console.log(`Stripped (named/invalid color → fill="none"): ${strippedCount}`);
console.log(`Output: ${output}`);
console.log(`Size: ${(Buffer.byteLength(result) / 1024).toFixed(1)} KB`);
