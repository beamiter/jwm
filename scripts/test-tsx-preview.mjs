#!/usr/bin/env node
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, writeFile, symlink, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs, inside, inspect, serverConfig, requestGuard } from './jwm-tsx-preview.mjs';

const tool = fileURLToPath(new URL('./jwm-tsx-preview.mjs', import.meta.url));

test('explicit trust, argument validation, no implicit execution', () => {
  assert.throws(() => parseArgs(['--project']), /Missing/);
  assert.throws(() => parseArgs(['--project', '.', '--port', '1;echo']), /port/);
  assert.throws(() => parseArgs(['--project', '.', '--host', '0.0.0.0']), /Unknown/);
  assert.equal(parseArgs(['--project', '.', '--trust-project']).trust, true);
  const result = spawnSync(process.execPath, [tool, '--project', '/nonexistent'], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /requires --trust-project/);
});

test('lexical containment is not a sibling-prefix check', () => {
  assert.equal(inside('/a/project', '/a/project/src/App.tsx'), true);
  assert.equal(inside('/a/project', '/a/project-secret/key'), false);
  assert.equal(inside('/a/project', '/a/project/../secret'), false);
});

test('fixed configuration disables project execution hooks and environment exposure', () => {
  for (const major of [7, 8]) {
    const config = serverConfig({ root: '/project', entry: '/project/src/A.tsx', major }, { component: 'src/A.tsx', port: 0 }, '/tmp/cache');
    assert.equal(config.configFile, false);
    assert.equal(config.envFile, false);
    assert.equal(config.envDir, false);
    assert.deepEqual(config.envPrefix, []);
    assert.deepEqual(config.css.postcss, { plugins: [] });
    assert.equal(config.publicDir, false);
    assert.equal(config.server.host, '127.0.0.1');
    assert.equal(config.server.cors, false);
    assert.equal(config.server.fs.strict, true);
    assert.equal(config.server.open, false);
  }
});

test('inspection requires contained real entry and local dependencies without loading JS', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'jwm-tsx-inspect-'));
  try {
    await writeFile(path.join(root, 'package.json'), '{"scripts":{"preview":"must not run"}}');
    await writeFile(path.join(root, 'index.html'), '<div></div>');
    for (const name of ['vite', 'react', 'react-dom']) {
      const dir = path.join(root, 'node_modules', name);
      await mkdir(dir, { recursive: true });
      await writeFile(path.join(dir, 'package.json'), JSON.stringify({ name, version: name === 'vite' ? '8.3.8' : '19.3.1' }));
      await writeFile(path.join(dir, 'index.js'), 'throw new Error("must not execute")');
    }
    const info = await inspect({ project: root });
    assert.equal(info.root, root);
    assert.equal(info.major, 8);
    await symlink('/etc/passwd', path.join(root, 'escape.tsx'));
    await assert.rejects(inspect({ project: root, component: 'escape.tsx' }), /inside/);
    await assert.rejects(inspect({ project: root, component: '../outside.tsx' }));
    const check = spawnSync(process.execPath, [tool, '--project', root, '--check'], { encoding: 'utf8' });
    assert.equal(check.status, 0, check.stderr);
    assert.equal(JSON.parse(check.stdout).execution, false);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('request gate rejects traversal, symlinks, dotfiles and foreign origins', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'jwm-tsx-gate-'));
  try {
    await writeFile(path.join(root, '.env'), 'SECRET');
    await writeFile(path.join(root, 'index.html'), 'okay');
    await writeFile(path.join(root, 'secret.key'), 'PRIVATE');
    await symlink(path.join(root, 'secret.key'), path.join(root, 'alias.txt'));
    await mkdir(path.join(root, 'node_modules/.pnpm/pkg/node_modules/pkg'), { recursive: true });
    await writeFile(path.join(root, 'node_modules/.pnpm/pkg/node_modules/pkg/style.css'), 'body{}');
    await symlink('/etc/passwd', path.join(root, 'escape.txt'));
    await symlink(path.join(root, '.env'), path.join(root, 'hidden.txt'));
    let middleware;
    requestGuard(root, path.join(root, 'cache')).configureServer({
      httpServer: { address: () => ({ port: 1234 }) },
      middlewares: { use: fn => { middleware = fn; } },
    });
    const request = async (url, headers = {}, method = 'GET') => {
      let outcome = 'unknown';
      const response = { setHeader() {}, end() { outcome = 'denied'; } };
      await middleware({ url, method, headers: { host: '127.0.0.1:1234', ...headers } }, response, () => { outcome = 'next'; });
      return outcome;
    };
    assert.equal(await request('/index.html'), 'next');
    assert.equal(await request('/node_modules/.pnpm/pkg/node_modules/pkg/style.css'), 'next');
    assert.equal(await request('/@fs/' + root + '/node_modules/.pnpm/pkg/node_modules/pkg/style.css'), 'next');
    for (const url of ['/alias.txt', '/secret.key', '/.pnpm/private', '/@id//etc/passwd', '/.env', '/%2e%2e/etc/passwd', '/escape.txt', '/hidden.txt', '/%00', '/@fs/etc/passwd', '/bad%encoding']) {
      assert.equal(await request(url), 'denied', url);
    }
    assert.equal(await request('/', { host: 'attacker.test:1234' }), 'denied');
    assert.equal(await request('/', { origin: 'https://attacker.test' }), 'denied');
    assert.equal(await request('/', {}, 'POST'), 'denied');
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('SIGTERM during dependency startup removes temporary cache without a ready server', { timeout: 10000 }, async () => {
  const { spawn } = await import('node:child_process');
  const { once } = await import('node:events');
  const { readdir } = await import('node:fs/promises');
  const root = await mkdtemp(path.join(tmpdir(), 'jwm-tsx-startup-'));
  let child;
  try {
    await mkdir(path.join(root, 'tmp'));
    await writeFile(path.join(root, 'package.json'), '{}');
    await writeFile(path.join(root, 'index.html'), '<div></div>');
    for (const name of ['vite', 'react', 'react-dom']) {
      const dir = path.join(root, 'node_modules', name);
      await mkdir(dir, { recursive: true });
      await writeFile(path.join(dir, 'package.json'), JSON.stringify({ name, type: 'module', version: name === 'vite' ? '8.3.8' : '19.3.1' }));
    }
    const viteDir = path.join(root, 'node_modules/vite/dist/node');
    await mkdir(viteDir, { recursive: true });
    await writeFile(path.join(viteDir, 'index.js'), 'console.log("DEPENDENCY_STARTING"); await new Promise(r=>setTimeout(r,30000)); throw new Error("must have stopped");');
    child = spawn(process.execPath, [tool, '--project', root, '--trust-project'], { env: { ...process.env, TMPDIR: path.join(root, 'tmp') }, stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    const started = new Promise(resolve => child.stdout.on('data', chunk => {
      output += chunk;
      if (output.includes('DEPENDENCY_STARTING')) resolve();
    }));
    await started;
    assert.equal((await readdir(path.join(root, 'tmp'))).length, 1);
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await exited;
    assert.equal(child.exitCode, 0);
    assert.deepEqual(await readdir(path.join(root, 'tmp')), []);
  } finally {
    if (child && child.exitCode === null) child.kill('SIGKILL');
    await rm(root, { recursive: true, force: true });
  }
});

test('Notes documentation links the shipped manual launcher with separate check and trust steps', async () => {
  const { readFile } = await import('node:fs/promises');
  const notes = await readFile(new URL('../docs/native-apps.md', import.meta.url), 'utf8');
  const preview = await readFile(new URL('../docs/tsx-preview.md', import.meta.url), 'utf8');
  assert.ok(notes.includes('[TSX preview launcher](tsx-preview.md)'));
  assert.ok(preview.includes('(native-apps.md#tsx-preview-in-an-external-browser)'));
  assert.ok(notes.includes('### TSX preview in an external browser'));
  assert.ok(!notes.includes('separate isolated runtime and remains planned'));
  const commands = [...notes.matchAll(/^\s*~\/\.local\/bin\/jwm-tsx-preview (.+)$/gm)]
    .map(match => parseArgs(match[1].trim().split(/\s+/)));
  assert.equal(commands.length, 2);
  assert.equal(commands[0].check, true);
  assert.equal(commands[0].trust, false);
  assert.equal(commands[0].open, false);
  assert.equal(commands[1].check, false);
  assert.equal(commands[1].trust, true);
  assert.equal(commands[1].open, true);
  assert.equal(commands[0].project, commands[1].project);
  assert.equal(commands[0].component, commands[1].component);
});
