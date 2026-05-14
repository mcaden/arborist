import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { basename, dirname, extname, isAbsolute, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const DEFAULT_HOST = '127.0.0.1';
const DEFAULT_PORT = 4173;
const FALLBACK_BASE = '/arborist';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const websiteRoot = join(repoRoot, 'website');

const contentTypes = new Map([
  ['.css', 'text/css; charset=utf-8'],
  ['.html', 'text/html; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.svg', 'image/svg+xml; charset=utf-8'],
  ['.txt', 'text/plain; charset=utf-8'],
  ['.webmanifest', 'application/manifest+json; charset=utf-8'],
  ['.xml', 'application/xml; charset=utf-8'],
]);

function usage() {
  return `Usage: pnpm run website:dev -- [--host <host>] [--port <port>]

Serves website/ as static files for local testing.
The server maps both / and /arborist/ to website/ so the GitHub Pages fallback path can be checked locally.
`;
}

function readOptionValue(argv, index, optionName) {
  const value = argv[index + 1];
  if (!value || value.startsWith('--')) {
    throw new Error(`Missing value for ${optionName}`);
  }
  return value;
}

function parseArgs(argv) {
  let host = DEFAULT_HOST;
  let port = DEFAULT_PORT;

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      process.stdout.write(usage());
      process.exitCode = 0;
      return null;
    }
    if (arg === '--host') {
      host = readOptionValue(argv, i, arg);
      i += 1;
      continue;
    }
    if (arg.startsWith('--host=')) {
      host = arg.slice('--host='.length);
      continue;
    }
    if (arg === '--port') {
      port = parsePort(readOptionValue(argv, i, arg));
      i += 1;
      continue;
    }
    if (arg.startsWith('--port=')) {
      port = parsePort(arg.slice('--port='.length));
      continue;
    }
    throw new Error(`Unknown option: ${arg}`);
  }

  return { host, port };
}

function parsePort(value) {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`Invalid port: ${value}`);
  }
  return port;
}

function contentTypeFor(filePath) {
  if (basename(filePath) === 'CNAME') {
    return 'text/plain; charset=utf-8';
  }
  return contentTypes.get(extname(filePath).toLowerCase()) ?? 'application/octet-stream';
}

function isInsideWebsiteRoot(filePath) {
  const rel = relative(websiteRoot, filePath);
  return rel === '' || (!rel.startsWith('..') && !isAbsolute(rel));
}

function sendText(res, status, body) {
  res.writeHead(status, {
    'Cache-Control': 'no-store',
    'Content-Type': 'text/plain; charset=utf-8',
    'X-Content-Type-Options': 'nosniff',
  });
  res.end(body);
}

function resolveRequestPath(pathname) {
  if (pathname === FALLBACK_BASE) {
    return { redirectTo: `${FALLBACK_BASE}/` };
  }

  const strippedPath = pathname.startsWith(`${FALLBACK_BASE}/`) ? pathname.slice(FALLBACK_BASE.length) : pathname;
  const normalizedPath = strippedPath === '/' ? '/index.html' : strippedPath;
  let decodedPath;
  try {
    decodedPath = decodeURIComponent(normalizedPath);
  } catch {
    return { status: 400, message: 'Bad request: invalid URL encoding.' };
  }

  if (decodedPath.includes('\0')) {
    return { status: 400, message: 'Bad request: null bytes are not allowed.' };
  }

  const filePath = resolve(websiteRoot, `.${decodedPath}`);
  if (!isInsideWebsiteRoot(filePath)) {
    return { status: 403, message: 'Forbidden.' };
  }

  return { filePath };
}

async function fileForRequest(pathname) {
  const resolved = resolveRequestPath(pathname);
  if (!resolved.filePath) {
    return resolved;
  }

  try {
    const info = await stat(resolved.filePath);
    if (info.isDirectory()) {
      const indexPath = join(resolved.filePath, 'index.html');
      if (!isInsideWebsiteRoot(indexPath)) {
        return { status: 403, message: 'Forbidden.' };
      }
      return { filePath: indexPath };
    }
    if (!info.isFile()) {
      return { status: 404, message: 'Not found.' };
    }
    return resolved;
  } catch (err) {
    if (err && typeof err === 'object' && 'code' in err && err.code === 'ENOENT') {
      return { status: 404, message: 'Not found.' };
    }
    throw err;
  }
}

async function handleRequest(req, res, host) {
  if (req.method !== 'GET' && req.method !== 'HEAD') {
    res.writeHead(405, { Allow: 'GET, HEAD' });
    res.end();
    return;
  }

  const url = new URL(req.url ?? '/', `http://${host}`);
  const result = await fileForRequest(url.pathname);

  if (result.redirectTo) {
    res.writeHead(301, { Location: result.redirectTo });
    res.end();
    return;
  }
  if (!result.filePath) {
    sendText(res, result.status ?? 500, result.message ?? 'Internal server error.');
    return;
  }

  const body = await readFile(result.filePath);
  res.writeHead(200, {
    'Cache-Control': 'no-store',
    'Content-Length': body.length,
    'Content-Type': contentTypeFor(result.filePath),
    'X-Content-Type-Options': 'nosniff',
  });
  res.end(req.method === 'HEAD' ? undefined : body);
}

function startServer({ host, port }) {
  const server = createServer((req, res) => {
    handleRequest(req, res, host).catch((err) => {
      process.stderr.write(`Website server error: ${err instanceof Error ? err.message : String(err)}\n`);
      if (res.headersSent) {
        res.destroy();
        return;
      }
      sendText(res, 500, 'Internal server error.');
    });
  });

  server.on('error', (err) => {
    process.stderr.write(`Unable to start website server: ${err instanceof Error ? err.message : String(err)}\n`);
    process.exitCode = 1;
  });

  server.listen(port, host, () => {
    process.stdout.write(`Serving ${websiteRoot}\n`);
    process.stdout.write(`  Local URL: http://${host}:${port}/\n`);
    process.stdout.write(`  Pages fallback path: http://${host}:${port}${FALLBACK_BASE}/\n`);
    process.stdout.write('Press Ctrl+C to stop.\n');
  });
}

try {
  const options = parseArgs(process.argv.slice(2));
  if (options) {
    startServer(options);
  }
} catch (err) {
  process.stderr.write(`${err instanceof Error ? err.message : String(err)}\n\n${usage()}`);
  process.exitCode = 1;
}
