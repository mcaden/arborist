import { readFileSync } from 'fs';

function getPathBounds(d) {
  let x = 0,
    y = 0,
    minX = Infinity,
    minY = Infinity,
    maxX = -Infinity,
    maxY = -Infinity;
  const update = (px, py) => {
    minX = Math.min(minX, px);
    minY = Math.min(minY, py);
    maxX = Math.max(maxX, px);
    maxY = Math.max(maxY, py);
  };
  const tokens = d.match(/[A-Za-z]|-?\d+(?:\.\d+)?/g);
  if (!tokens) return { minX: 0, minY: 0, maxX: 601, maxY: 601, area: 601 * 601 };
  let cmd = 'M',
    i = 0;
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
      const nx = parseFloat(tokens[i]);
      const ny = parseFloat(tokens[i + 1] || '0');
      x = isRel ? x + nx : nx;
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 2;
    } else if (C === 'H') {
      const nx = parseFloat(tokens[i]);
      x = isRel ? x + nx : nx;
      update(x, y);
      i += 1;
    } else if (C === 'V') {
      const ny = parseFloat(tokens[i]);
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 1;
    } else if (C === 'C') {
      for (let p = 0; p < 3; p++) {
        const nx = parseFloat(tokens[i]);
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
        const nx = parseFloat(tokens[i]);
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
      i += 5;
      const nx = parseFloat(tokens[i]);
      const ny = parseFloat(tokens[i + 1] || '0');
      x = isRel ? x + nx : nx;
      y = isRel ? y + ny : ny;
      update(x, y);
      i += 2;
    } else if (C === 'Z') {
      /* no params */
    } else {
      i++;
    }
  }
  return { minX, minY, maxX, maxY, area: (maxX - minX) * (maxY - minY) };
}

const svg = readFileSync(process.argv[2], 'utf8');

const paths = [];
svg.replace(/<path\s+d="([^"]*)"\s+fill="(#[A-Fa-f0-9]+)"/g, (m, d, _fill) => {
  paths.push(getPathBounds(d));
});

console.log('maxY distribution:');
const buckets = [0, 100, 200, 300, 350, 400, 450, 500, 601];
for (let b = 0; b < buckets.length - 1; b++) {
  const count = paths.filter((p) => p.maxY >= buckets[b] && p.maxY < buckets[b + 1]).length;
  console.log(`  maxY ${buckets[b]}-${buckets[b + 1]}: ${count}`);
}

console.log('\narea distribution:');
const areaBuckets = [0, 100, 500, 1000, 2000, 5000, 10000, 50000, 1000000];
for (let b = 0; b < areaBuckets.length - 1; b++) {
  const count = paths.filter((p) => p.area >= areaBuckets[b] && p.area < areaBuckets[b + 1]).length;
  console.log(`  area ${areaBuckets[b]}-${areaBuckets[b + 1]}: ${count}`);
}
