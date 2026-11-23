#!/usr/bin/env node
// Uses only this repository's explicitly installed verification dependencies.
import assert from 'node:assert/strict';
import { cp, mkdtemp, mkdir, writeFile, readFile, readdir, symlink, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';
import { chromium } from '../tests/tsx-preview/node_modules/playwright/index.mjs';

const tool = fileURLToPath(new URL('./jwm-tsx-preview.mjs', import.meta.url));
const dependencyRoot = fileURLToPath(new URL('../tests/tsx-preview/node_modules', import.meta.url));
const root = await mkdtemp(path.join(tmpdir(), 'jwm-tsx-e2e-'));
const children = new Set();
let browser;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const component = label => `import {useState} from 'react'; export default function App(){const [n,setN]=useState(0); return <button onClick={()=>setN(n+1)}>${label} {n}</button>}`;

async function start(extra = []) {
  const child = spawn(process.execPath, [tool, '--project', root, '--trust-project', ...extra], {
    env: { ...process.env, VITE_SHOULD_NOT_LEAK: 'parent-secret-marker' }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  children.add(child);
  let output = '';
  child.stdout.on('data', chunk => { output += chunk; });
  child.stderr.on('data', chunk => { output += chunk; });
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) {
    const match = output.match(/JWM_PREVIEW_URL=(http:\/\/127\.0\.0\.1:\d+\/)/);
    if (match) return { child, url: match[1], logs: () => output };
    if (child.exitCode !== null) throw new Error(`Preview exited ${child.exitCode}: ${output}`);
    await sleep(40);
  }
  throw new Error(`Preview startup timeout: ${output}`);
}

async function stop(run) {
  const exited = once(run.child, 'exit');
  run.child.kill('SIGTERM');
  await Promise.race([exited, sleep(10000).then(() => { throw new Error('Preview did not stop'); })]);
  children.delete(run.child);
  assert.equal(run.child.exitCode, 0, run.logs());
  await assert.rejects(fetch(run.url), 'Stopped preview must no longer accept requests');
}

try {
  await cp(dependencyRoot, path.join(root, 'node_modules'), { recursive: true });
  await mkdir(path.join(root, 'src'));
  await mkdir(path.join(root, 'public'));
  await writeFile(path.join(root, 'package.json'), JSON.stringify({ type: 'module', scripts: { dev: 'touch MUST_NOT_RUN' } }));
  await writeFile(path.join(root, 'index.html'), '<!doctype html><html><head></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>');
  await writeFile(path.join(root, 'src/main.tsx'), `import React from 'react'; import {createRoot} from 'react-dom/client'; import App from './App'; import './style.css'; createRoot(document.getElementById('root')).render(<App/>); console.log(import.meta.env.VITE_SHOULD_NOT_LEAK,import.meta.env.VITE_ENV_SECRET);`);
  await writeFile(path.join(root, 'src/App.tsx'), component('Project'));
  await writeFile(path.join(root, 'src/style.css'), 'button { color: rgb(1, 2, 3); }');
  await writeFile(path.join(root, 'vite.config.mjs'), 'throw new Error("PROJECT_CONFIG_EXECUTED")');
  await writeFile(path.join(root, 'postcss.config.cjs'), 'throw new Error("POSTCSS_CONFIG_EXECUTED")');
  await writeFile(path.join(root, '.env'), 'VITE_ENV_SECRET=env-secret-marker');
  await writeFile(path.join(root, 'public/not-served.txt'), 'public-marker');
  await writeFile(path.join(root, 'secret.key'), 'private-key-marker');
  await symlink(path.join(root, 'secret.key'), path.join(root, 'alias.txt'));
  await symlink('/etc/passwd', path.join(root, 'outside.txt'));
  const cachesBefore = new Set((await readdir(tmpdir())).filter(name => name.startsWith('jwm-tsx-preview-')));
  browser = await chromium.launch({ headless: true, ...(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {}) });
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(String(error)));
  let run = await start();
  await page.goto(run.url);
  await page.getByRole('button', { name: 'Project 0' }).waitFor();
  await page.getByRole('button').click();
  await page.getByRole('button', { name: 'Project 1' }).waitFor();
  assert.equal(await page.getByRole('button').evaluate(el => getComputedStyle(el).color), 'rgb(1, 2, 3)');
  const compiled = await (await fetch(`${run.url}src/main.tsx`)).text();
  assert.ok(!compiled.includes('parent-secret-marker') && !compiled.includes('env-secret-marker'));
  for (const resource of ['.env', 'alias.txt', 'outside.txt', '@fs/etc/passwd', '@id//etc/passwd', '%252e%252e/etc/passwd']) {
    assert.equal((await fetch(run.url + resource)).status, 403, resource);
  }
  assert.equal((await fetch(run.url, { headers: { Origin: 'https://attacker.example' } })).status, 403);
  assert.ok(!(await (await fetch(`${run.url}not-served.txt`)).text()).includes('public-marker'));
  await writeFile(path.join(root, 'src/App.tsx'), component('Updated'));
  await page.getByRole('button', { name: 'Updated 0' }).waitFor();
  assert.deepEqual(errors, []);
  await stop(run);

  run = await start(['--component', 'src/App.tsx']);
  await page.goto(run.url);
  await page.getByRole('button', { name: 'Updated 0' }).waitFor();
  await page.getByRole('button').click();
  await page.getByRole('button', { name: 'Updated 1' }).waitFor();
  await writeFile(path.join(root, 'src/App.tsx'), 'export default function Broken(){return <div>');
  await page.locator('vite-error-overlay').waitFor();
  await writeFile(path.join(root, 'src/App.tsx'), component('Recovered'));
  await page.getByRole('button', { name: 'Recovered 0' }).waitFor();
  await writeFile(path.join(root, 'src/App.tsx'),
    'export default function Broken(){throw new Error("<mark>render failure</mark>");}');
  const failure = page.getByRole('alert');
  await failure.getByRole('heading', { name: 'Component preview failed' }).waitFor();
  await failure.getByText('<mark>render failure</mark>', { exact: true }).waitFor();
  assert.equal(await failure.locator('mark').count(), 0, 'error details must be rendered as plain text');
  await failure.getByText('Fix the component and save to try again.').waitFor();
  await writeFile(path.join(root, 'src/App.tsx'), component('Runtime recovered'));
  await page.getByRole('button', { name: 'Runtime recovered 0' }).waitFor();
  assert.equal(await failure.count(), 0);

  await writeFile(path.join(root, 'src/App.tsx'), 'export function NamedOnly(){return <p>Named only</p>;}');
  await failure.getByText('Export the preview component as default.').waitFor();
  await writeFile(path.join(root, 'src/App.tsx'), component('Default restored'));
  await page.getByRole('button', { name: 'Default restored 0' }).waitFor();
  assert.equal(await failure.count(), 0);
  console.log('PASS: component render error and missing default export are visible and recover on save');
  const initMessage = 'component initialization failed';
  const appPath = path.join(root, 'src/App.tsx');
  await writeFile(appPath,
    'throw new Error("component initialization failed"); export default function App(){return <div>not mounted</div>;}');
  // Start from a fresh document, with no state or overlay from the preceding mode.
  const initPage = await browser.newPage();
  const observedError = initPage.waitForEvent('pageerror', {
    predicate: error => error.message === initMessage, timeout: 10000,
  });
  await initPage.goto(run.url);
  await observedError;
  assert.equal(await initPage.locator('#root').textContent(), '');
  assert.equal(await initPage.locator('vite-error-overlay').count(), 0);
  assert.equal(await initPage.getByRole('alert').count(), 0);
  console.log('PASS (baseline defect): module initialization exception visible to pageerror but preview surface is blank');
  await writeFile(appPath, component('Initialization fixed'));
  await initPage.getByRole('button', { name: 'Initialization fixed 0' }).waitFor();
  await initPage.close();
  await stop(run);
  assert.equal(await readFile(path.join(root, '.env'), 'utf8'), 'VITE_ENV_SECRET=env-secret-marker');
  await assert.rejects(readFile(path.join(root, 'MUST_NOT_RUN')));
  const remaining = (await readdir(tmpdir())).filter(name => name.startsWith('jwm-tsx-preview-') && !cachesBefore.has(name));
  assert.deepEqual(remaining, [], 'Preview cache must be removed on stop');
  console.log('PASS: real React project/component rendering, interaction, saved-file reload, syntax-error recovery, config/env/URL boundaries, SIGTERM cleanup');
} finally {
  await browser?.close();
  for (const child of children) {
    child.kill('SIGTERM');
    await Promise.race([once(child, 'exit'), sleep(3000)]);
    if (child.exitCode === null) child.kill('SIGKILL');
  }
  await rm(root, { recursive: true, force: true });
}
