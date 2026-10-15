import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { stripTypeScriptTypes } from 'node:module';
import test from 'node:test';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = process.argv[2] ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const frontends = [
  { name: 'Solid', file: 'bars/tauri_solid_bar/src/App.tsx', kind: 'setter' },
  { name: 'Svelte', file: 'bars/tauri_svelte_bar/src/App.svelte', kind: 'assignment' },
  { name: 'Vue', file: 'bars/tauri_vue_bar/src/App.vue', kind: 'ref' },
];

for (const frontend of frontends) {
  const source = await readFile(path.join(root, frontend.file), 'utf8');
  const pattern = frontend.kind === 'ref'
    ? /\(async \(\) => \{([\s\S]*?)\n  \}\)\(\)\.catch\(/
    : /const initialize = async \(\) => \{([\s\S]*?)\n    \};\n\n    initialize\(\)/;
  const block = source.match(pattern);
  assert.ok(block, `extract the production ${frontend.name} initializer without loading UI or Tauri`);
  const initializer = stripTypeScriptTypes(`async function initialize() {${block[1]}\n}`);

  function makeRuntime({ queryScale, stopAtReady = false, listenDelay = null }) {
    const calls = [];
    const factory = new Function('listen', 'getCurrentWindow', 'invoke', 'console', `
      let cancelled = false, revision = null, snapshotBarOrigin = null, unlisten;
      let snapshot = null;
      let scaleFactor = ${frontend.kind === 'ref' ? '{ value: null }' : 'null'};
      const setScaleFactor = value => { scaleFactor = value; };
      const setSnapshot = value => { snapshot = value; };
      ${initializer}
      return {
        initialize,
        cancel: () => { cancelled = true; },
        get optionalValue() { return ${frontend.kind === 'ref' ? 'scaleFactor.value' : 'scaleFactor'}; }
      };
    `);
    let runtime;
    runtime = factory(
      async () => {
        calls.push('listen');
        if (listenDelay) await listenDelay;
        return () => calls.push('unlisten');
      },
      () => ({ scaleFactor: () => { calls.push('scale'); return queryScale(); } }),
      async command => { calls.push(command); if (stopAtReady) runtime.cancel(); },
      { error: () => calls.push('optional-error') },
    );
    return { runtime, calls };
  }

  test(`${frontend.name}: rejected optional scale query does not prevent readiness`, async () => {
    const { runtime, calls } = makeRuntime({ queryScale: async () => { throw new Error('synthetic query failure'); } });
    await runtime.initialize();
    assert.deepEqual(calls, ['listen', 'frontend_ready', 'scale', 'optional-error']);
    assert.equal(runtime.optionalValue, null);
  });

  test(`${frontend.name}: pending optional scale query does not delay readiness`, async () => {
    let resolve;
    const { runtime, calls } = makeRuntime({ queryScale: () => new Promise(r => { resolve = r; }) });
    const completion = runtime.initialize();
    for (let turn = 0; turn < 5; ++turn) await Promise.resolve();
    assert.ok(calls.includes('frontend_ready'), JSON.stringify(calls));
    resolve(2);
    await completion;
    assert.equal(runtime.optionalValue, 2);
  });

  test(`${frontend.name}: cancellation after readiness skips optional query`, async () => {
    const { runtime, calls } = makeRuntime({ queryScale: async () => 2, stopAtReady: true });
    await runtime.initialize();
    assert.deepEqual(calls, ['listen', 'frontend_ready']);
    assert.equal(runtime.optionalValue, null);
  });

  test(`${frontend.name}: late optional result cannot update disposed state`, async () => {
    let resolve;
    const { runtime } = makeRuntime({ queryScale: () => new Promise(r => { resolve = r; }) });
    const completion = runtime.initialize();
    for (let turn = 0; turn < 5; ++turn) await Promise.resolve();
    runtime.cancel();
    resolve(2);
    await completion;
    assert.equal(runtime.optionalValue, null);
  });

  test(`${frontend.name}: listener resolving after disposal is immediately released`, async () => {
    let resolve;
    const listenDelay = new Promise(r => { resolve = r; });
    const { runtime, calls } = makeRuntime({ queryScale: async () => 2, listenDelay });
    const completion = runtime.initialize();
    runtime.cancel();
    resolve();
    await completion;
    assert.deepEqual(calls, ['listen', 'unlisten']);
    assert.equal(runtime.optionalValue, null);
  });
}
