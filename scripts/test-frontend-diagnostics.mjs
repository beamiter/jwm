#!/usr/bin/env node
// Offline execution of the production probe: no browser, X server, or IPC needed.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source = readFileSync(new URL('../crates/xbar_core/crates/xbar_tauri/src/frontend_diagnostics.js', import.meta.url), 'utf8');
function fixture(rejectStage) {
  const reports = [], frames = [];
  let mutation, ready = false, disconnected = false;
  const context = {
    window: { addEventListener() {}, __TAURI_INTERNALS__: { invoke(command, payload) {
      assert.equal(command, 'frontend_diagnostic');
      reports.push(payload);
      return payload.stage === rejectStage ? Promise.reject(new Error('test rejection')) : Promise.resolve();
    } } },
    console: { error() {} },
    document: { querySelector(selector) { return ready ? { textContent: 'workspace 1' } : (selector === '.button-row' ? { textContent: 'Loading...' } : null); } },
    MutationObserver: class { constructor(callback) { mutation = callback; } observe() {} disconnect() { disconnected = true; } },
    requestAnimationFrame(callback) { frames.push(callback); },
  };
  vm.runInNewContext(source, context);
  return { reports, frames, stages: () => reports.map(item => item.stage), mutate(value = true) { ready = value; mutation(); }, frame() { assert(frames.length); frames.shift()(); }, disconnected: () => disconnected };
}
test('DOM prerequisite and original double-rAF rendered ordering', () => {
  const f = fixture();
  f.mutate(false);
  assert.deepEqual(f.stages(), ['boot']);
  assert.equal(f.frames.length, 0);
  f.mutate(); f.mutate();
  assert.equal(f.disconnected(), true);
  assert.deepEqual(f.stages(), ['boot', 'dom-ready']);
  assert.equal(f.frames.length, 1);
  f.frame();
  assert.deepEqual(f.stages(), ['boot', 'dom-ready', 'raf-first']);
  f.frame();
  assert.deepEqual(f.stages(), ['boot', 'dom-ready', 'raf-first', 'raf-second', 'rendered']);
  assert.equal(f.frames.length, 0);
});
test('stalled first or second rAF never announces rendered', () => {
  const f = fixture(); f.mutate();
  assert.deepEqual(f.stages(), ['boot', 'dom-ready']);
  f.frame();
  assert.deepEqual(f.stages(), ['boot', 'dom-ready', 'raf-first']);
  assert(!f.stages().includes('rendered'));
});
test('rejected rendered reports dedicated evidence, not an error or retry', async () => {
  const f = fixture('rendered'); f.mutate(); f.frame(); f.frame();
  await Promise.resolve(); await Promise.resolve();
  assert.deepEqual(f.stages(), ['boot', 'dom-ready', 'raf-first', 'raf-second', 'rendered', 'render-report-rejected']);
  assert.equal(f.reports.at(-1).message, 'test rejection');
});
test('a rejected probe neither suppresses rendered nor recursively reports errors', async () => {
  for (const stage of ['dom-ready', 'raf-first', 'raf-second']) {
    const f = fixture(stage); f.mutate(); f.frame(); f.frame();
    await Promise.resolve(); await Promise.resolve();
    assert.deepEqual(f.stages(), ['boot', 'dom-ready', 'raf-first', 'raf-second', 'rendered']);
  }
});
