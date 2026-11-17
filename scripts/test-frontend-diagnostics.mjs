#!/usr/bin/env node
// Offline execution of the production probe: no browser, X server, or IPC needed.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source = readFileSync(new URL('../crates/xbar_core/crates/xbar_tauri/src/frontend_diagnostics.js', import.meta.url), 'utf8');
function fixture(rejectStage) {
  const reports = [], frames = [], listeners = {};
  let mutation, ready = false, disconnected = false;
  const context = {
    window: { innerWidth: 1270, innerHeight: 42, addEventListener() {}, __TAURI_INTERNALS__: { invoke(command, payload) {
      assert.equal(command, 'frontend_diagnostic');
      reports.push(payload);
      return payload.stage === rejectStage ? Promise.reject(new Error('test rejection')) : Promise.resolve();
    } } },
    console: { error() {} },
    document: { visibilityState: "visible", hidden: false, addEventListener(name, callback) { listeners[name] = callback; }, querySelector(selector) { return ready ? { textContent: 'workspace 1' } : (selector === '.button-row' ? { textContent: 'Loading...' } : null); } },
    MutationObserver: class { constructor(callback) { mutation = callback; } observe() {} disconnect() { disconnected = true; } },
    setTimeout() { return 1; }, clearTimeout() {},
    requestAnimationFrame(callback) { frames.push(callback); },
  };
  vm.runInNewContext(source, context);
  return { context, listeners, reports, frames, stages: () => reports.map(item => item.stage), mutate(value = true) { ready = value; mutation(); }, frame() { assert(frames.length); frames.shift()(); }, disconnected: () => disconnected };
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

test('DOM and visibility changes report exact viewport and bounded snapshots', () => {
  const f = fixture(); f.mutate();
  assert.deepEqual(JSON.parse(f.reports.find(r => r.stage === 'dom-ready').message),
    {visibilityState: 'visible', hidden: false, width: 1270, height: 42});
  f.context.document.hidden = true; f.context.document.visibilityState = 'hidden';
  for (let i = 0; i < 20; i++) f.listeners.visibilitychange();
  const changes = f.reports.filter(r => r.stage === 'visibility');
  assert.equal(changes.length, 16);
  assert.equal(JSON.parse(changes[0].message).hidden, true);
  f.frame(); f.frame(); assert(f.stages().includes('rendered'));
});
test('visibility collection throwing cannot suppress rendered', () => {
  const f = fixture();
  Object.defineProperty(f.context.document, 'hidden', { get() { throw Error('probe failure'); } });
  f.mutate(); f.frame(); f.frame();
  assert.deepEqual(f.stages(), ['boot', 'raf-first', 'raf-second', 'rendered']);
});
test('rejected visibility report cannot alter frame acknowledgement', async () => {
  const f = fixture('visibility');
  f.mutate(); f.listeners.visibilitychange(); f.frame(); f.frame();
  await Promise.resolve(); await Promise.resolve();
  assert.equal(f.stages().filter(stage => stage === 'rendered').length, 1);
  assert(!f.stages().includes('error'));
  assert(!f.stages().includes('render-report-rejected'));
});
