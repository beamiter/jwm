#!/usr/bin/env node
// Explicit, trusted-project preview. This is a development server, NOT a sandbox.
import { realpath, readFile, stat, mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawn } from 'node:child_process';

const HELP = `JWM TSX preview (external browser, trusted projects only)
Usage: jwm-tsx-preview --project DIR --trust-project [--component FILE.tsx] [--port N] [--open]
       jwm-tsx-preview --project DIR --check [--component FILE.tsx]

Uses an already installed, project-local Vite 7/8 and React. Installs nothing.
Default: preview the project's index.html; --component mounts a default-exported
React component without props. Save in the editor to update the browser.
--check validates paths/dependencies without loading Vite or starting a server.
--trust-project is required for execution: dependencies run as your user and
browser code can use the network. This is NOT isolation for untrusted code.
Vite/PostCSS configs, package scripts, .env files and public/ are not loaded.
Only 127.0.0.1 is served. Ctrl+C stops the server and removes its temporary cache.
`;

export function parseArgs(args) {
  const options = { port: 0, open: false, trust: false, check: false };
  for (let i = 0; i < args.length; i++) {
    const key = args[i];
    if (key === '--help' || key === '-h') return { help: true };
    if (key === '--trust-project') options.trust = true;
    else if (key === '--open') options.open = true;
    else if (key === '--check') options.check = true;
    else if (['--project', '--component', '--port'].includes(key)) {
      if (!args[i + 1] || args[i + 1].startsWith('--')) throw new Error(`Missing value: ${key}`);
      options[key.slice(2)] = args[++i];
    } else throw new Error(`Unknown option: ${key}`);
  }
  if (!options.project) throw new Error('--project DIR is required.');
  if (!/^\d+$/.test(String(options.port)) || Number(options.port) > 65535) {
    throw new Error('--port must be 0 (automatic) or 1–65535.');
  }
  options.port = Number(options.port);
  return options;
}

export function inside(root, candidate) {
  const relative = path.relative(root, candidate);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

function hasPrivateSegment(root, target) {
  return path.relative(root, target).split(path.sep).some((part, index, parts) =>
    part.startsWith('.') && !(part === '.pnpm' && index > 0 && parts[index - 1] === 'node_modules'));
}

export async function inspect(options) {
  const root = await realpath(path.resolve(options.project));
  if (!(await stat(root)).isDirectory()) throw new Error('Project must be a directory.');
  const manifest = path.join(root, 'package.json');
  if (!inside(root, await realpath(manifest))) throw new Error('package.json must stay inside the project.');
  const pkg = JSON.parse(await readFile(manifest, 'utf8'));
  const require = createRequire(manifest);
  const packages = {};
  for (const name of ['vite', 'react', 'react-dom']) {
    let packagePath;
    try { packagePath = await realpath(require.resolve(`${name}/package.json`)); }
    catch { throw new Error(`Missing local ${name}. Install project dependencies yourself before previewing.`); }
    if (!inside(root, packagePath)) throw new Error(`${name} must be installed within this project (no global/parent dependency fallback).`);
    packages[name] = { ...JSON.parse(await readFile(packagePath, 'utf8')), directory: path.dirname(packagePath) };
  }
  const major = Number(packages.vite.version.split('.')[0]);
  if (![7, 8].includes(major)) throw new Error(`Vite ${packages.vite.version} is not supported; this tool supports Vite 7/8.`);
  const selected = options.component ? path.resolve(root, options.component) : path.join(root, 'index.html');
  const entry = await realpath(selected);
  if (!inside(root, selected) || !inside(root, entry)) throw new Error('Entry must stay inside the project, including symlink targets.');
  if (!(await stat(entry)).isFile()) throw new Error('Entry must be a regular file.');
  if (options.component && !/\.(tsx|jsx)$/i.test(entry)) throw new Error('--component requires a .tsx or .jsx file.');
  return { root, entry, pkg, packages, major };
}

function componentPlugin(entryUrl) {
  const id = 'virtual:jwm-component';
  const html = '<!doctype html><html><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>JWM TSX preview</title></head><body><div id="root"></div><script type="module" src="/@id/virtual:jwm-component"></script></body></html>';
  return {
    name: 'jwm-component-entry',
    resolveId(source) { if (source === id) return `\0${id}`; },
    load(source) {
      if (source !== `\0${id}`) return;
      return `import React from 'react';
import { createRoot } from 'react-dom/client';
import * as PreviewModule from ${JSON.stringify(entryUrl)};
const h = React.createElement;
function failure(message) {
  return h('main', { role: 'alert' },
    h('h1', null, 'Component preview failed'),
    h('pre', { style: { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' } }, message),
    h('p', null, 'Fix the component and save to try again.'));
}
class PreviewBoundary extends React.Component {
  state = { message: null };
  static getDerivedStateFromError(error) {
    const message = error instanceof Error ? error.message : '';
    return { message: message.slice(0, 1000) || 'The component failed while rendering.' };
  }
  render() {
    return this.state.message === null ? this.props.children : failure(this.state.message);
  }
}
const Component = PreviewModule.default;
createRoot(document.getElementById('root')).render(Component == null
  ? failure('Export the preview component as default.')
  : h(PreviewBoundary, null, h(Component)));
`;
    },
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (req.url?.split('?')[0] !== '/' && req.url?.split('?')[0] !== '/index.html') return next();
        try {
          res.setHeader('Content-Type', 'text/html; charset=utf-8');
          res.end(await server.transformIndexHtml('/', html));
        } catch (error) { next(error); }
      });
    },
  };
}

export function requestGuard(root, cache) {
  return {
    name: 'jwm-local-preview-boundary',
    enforce: 'pre',
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const reject = () => { res.statusCode = 403; res.end('JWM preview: request denied'); };
        const address = server.httpServer?.address();
        const origin = address && typeof address !== 'string' ? `http://127.0.0.1:${address.port}` : '';
        if (!origin || `http://${req.headers.host}` !== origin || (req.headers.origin && req.headers.origin !== origin)) return reject();
        if (!['GET', 'HEAD'].includes(req.method)) return reject();
        res.setHeader('Cache-Control', 'no-store');
        res.setHeader('X-Content-Type-Options', 'nosniff');
        res.setHeader('Referrer-Policy', 'no-referrer');
        try {
          const raw = decodeURIComponent((req.url || '/').split('?')[0]);
          if (raw.includes('\0') || raw.includes('\\') || /%[0-9a-f]{2}/i.test(raw) || raw.split('/').includes('..')) return reject();
          if (['/@vite/client', '/@vite/env', '/@id/virtual:jwm-component', '/@id/__x00__virtual:jwm-component', '/@id/__vite-browser-external', '/@id/__x00__vite-browser-external'].includes(raw)) return next();
          if (raw.startsWith('/@id/')) return reject();
          const target = raw.startsWith('/@fs/') ? raw.slice(4) : path.join(root, raw);
          if (!inside(root, target) && !inside(cache, target)) return reject();
          if (/\.(pem|key|p12|pfx)$/i.test(target)) return reject();
          if (!inside(cache, target) && hasPrivateSegment(root, target)) return reject();
          // Check actual targets, not just lexical paths: a symlink cannot expose
          // a sibling checkout or a private file under a non-dot filename.
          try {
            const actual = await realpath(target);
            if (!inside(root, actual) && !inside(cache, actual)) return reject();
            if (/\.(pem|key|p12|pfx)$/i.test(actual)) return reject();
            if (!inside(cache, actual) && hasPrivateSegment(root, actual)) return reject();
          } catch (error) {
            if (!['ENOENT', 'ENOTDIR'].includes(error.code)) throw error;
          }
          next();
        } catch { reject(); }
      });
    },
  };
}

export function serverConfig(info, options, cache) {
  const relative = path.relative(info.root, info.entry).split(path.sep).map(encodeURIComponent).join('/');
  return {
    root: info.root,
    configFile: false,
    envFile: false,
    envDir: false,
    envPrefix: [],
    publicDir: false,
    cacheDir: cache,
    clearScreen: false,
    css: { postcss: { plugins: [] } },
    // Do not allow a project's jsx runtime setting to change this tool's React
    // contract. Vite 8 uses Oxc; Vite 7 uses esbuild.
    ...(info.major === 8 ? { oxc: { jsx: { runtime: 'automatic', importSource: 'react' } } }
      : { esbuild: { jsx: 'automatic', jsxImportSource: 'react' } }),
    plugins: [requestGuard(info.root, cache), ...(options.component ? [componentPlugin(`/${relative}`)] : [])],
    server: {
      host: '127.0.0.1', port: options.port, strictPort: true, open: false,
      cors: false, allowedHosts: [],
      fs: { strict: true, allow: [info.root, cache], deny: ['**/.git/**', '**/.env', '**/.env.*', '**/*.{pem,key,p12,pfx}'] },
    },
  };
}

export async function main(args) {
  const options = parseArgs(args);
  if (options.help) { console.log(HELP); return; }
  if (!options.check && !options.trust) throw new Error('Execution requires --trust-project. Dependencies run as your user; browser code can use the network. This is NOT a sandbox. Use --check for read-only validation.');
  const info = await inspect(options);
  if (options.check) {
    console.log(JSON.stringify({ project: info.root, entry: info.entry, vite: info.packages.vite.version, mode: options.component ? 'component' : 'project', execution: false }));
    return;
  }
  console.error('Trusted-project preview: NOT a sandbox. Local dependencies execute as your user; browser code may access the network. Package scripts, Vite/PostCSS configs and .env are not loaded. TypeScript configuration may still be read.');
  const cache = await mkdtemp(path.join(tmpdir(), 'jwm-tsx-preview-'));
  let server;
  let closing;
  const close = () => closing ||= (async () => {
    try { await server?.close(); } finally { await rm(cache, { recursive: true, force: true }); }
  })();
  const stop = () => { close().then(() => process.exit(0), error => { console.error(error); process.exit(1); }); };
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
  try {
    const vite = await import(pathToFileURL(path.join(info.packages.vite.directory, 'dist/node/index.js')).href);
    server = await vite.createServer(serverConfig(info, options, cache));
    await server.listen();
    const address = server.httpServer.address();
    const url = `http://127.0.0.1:${address.port}/`;
    console.log(`JWM_PREVIEW_URL=${url}`);
    console.log('Save source files to update. Ctrl+C stops preview. Browser preview is external, not embedded in jwm-notes.');
    if (options.open) {
      const browser = spawn('xdg-open', [url], { stdio: 'ignore' });
      browser.on('error', error => console.error(`Could not open browser: ${error.message}. Open ${url} manually.`));
      browser.on('exit', code => { if (code) console.error(`Browser opener exited ${code}; open ${url} manually.`); });
    }
  } catch (error) {
    await close();
    throw error;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch(error => { console.error(`JWM TSX preview: ${error.message}`); process.exitCode = 1; });
}
