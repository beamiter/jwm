import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import test from 'node:test';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = process.argv[2] ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const source = await readFile(path.join(root, 'bars/tauri_solid_bar/src/App.tsx'), 'utf8');
const block = source.match(/const initialize = async \(\) => \{([\s\S]*?)\n    \};\n\n    initialize\(\)/);
assert.ok(block, 'extract the production Solid initializer, without loading UI or Tauri');
const initializer = stripTypeScriptTypes(`async function initialize() {${block[1]}\n}`);
function makeRuntime({ scaleFactor, stopAtReady = false }) {
  const calls = [];
  const factory = new Function('listen','getCurrentWindow','invoke','setScaleFactor','setSnapshot','console', `
    let cancelled = false, revision = null, snapshotBarOrigin = null, unlisten;
    ${initializer}
    return { initialize, cancel: () => { cancelled = true; } };
  `);
  let runtime;
  runtime = factory(
    async () => { calls.push('listen'); return () => calls.push('unlisten'); },
    () => ({ scaleFactor: () => { calls.push('scale'); return scaleFactor(); } }),
    async cmd => { calls.push(cmd); if (stopAtReady) runtime.cancel(); },
    value => calls.push(`apply:${value}`),
    () => {},
    { error: () => calls.push('optional-error') },
  );
  return { ...runtime, calls };
}

test('a rejected optional scale-factor query does not prevent the bar handshake', async () => {
  const run = makeRuntime({ scaleFactor: async () => { throw new Error('synthetic query failure'); } });
  await run.initialize();
  assert.deepEqual(run.calls, ['listen', 'frontend_ready', 'scale', 'optional-error']);
});

test('a pending optional scale-factor query does not delay frontend_ready', async () => {
  let resolve;
  const run = makeRuntime({ scaleFactor: () => new Promise(r => { resolve = r; }) });
  const completion = run.initialize();
  for (let turn = 0; turn < 5; ++turn) await Promise.resolve();
  assert.ok(run.calls.includes('frontend_ready'), JSON.stringify(run.calls));
  resolve(2);
  await completion;
  assert.equal(run.calls.at(-1), 'apply:2');
});

test('cancellation after readiness skips the optional query', async () => {
  const run = makeRuntime({ scaleFactor: async () => 2, stopAtReady: true });
  await run.initialize();
  assert.deepEqual(run.calls, ['listen', 'frontend_ready']);
});

test('cancellation while the optional query is pending does not update disposed state', async () => {
  let resolve;
  const run = makeRuntime({ scaleFactor: () => new Promise(r => { resolve = r; }) });
  const completion = run.initialize();
  for (let turn = 0; turn < 5; ++turn) await Promise.resolve();
  run.cancel();
  resolve(2);
  await completion;
  assert.ok(!run.calls.includes('apply:2'));
});
