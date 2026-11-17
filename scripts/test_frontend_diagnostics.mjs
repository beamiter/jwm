import {readFileSync} from "node:fs";
import {runInNewContext} from "node:vm";
import assert from "node:assert/strict";
import test from "node:test";

const source = readFileSync(new URL("../crates/xbar_core/crates/xbar_tauri/src/frontend_diagnostics.js", import.meta.url), "utf8");
function harness({rejectRender = false, pendingRender = false} = {}) {
  const calls = [], frames = [], timers = new Map(), nodes = new Map();
  let mutation, disconnected = false, nextTimer = 0;
  const context = {
    window: {addEventListener() {}, __TAURI_INTERNALS__: {invoke(_, payload) {
      calls.push(payload);
      if (payload.stage === "rendered" && pendingRender) return new Promise(() => {});
      return payload.stage === "rendered" && rejectRender ? Promise.reject(new Error("test rejection")) : Promise.resolve();
    }}},
    document: {addEventListener() {}, readyState: "complete", visibilityState: "visible", querySelector: name => nodes.get(name)},
    console: {error() {}}, Promise,
    MutationObserver: class {
      constructor(callback) { mutation = callback; }
      observe(_, options) { assert.deepEqual(JSON.parse(JSON.stringify(options)), {childList: true, subtree: true}); }
      disconnect() { disconnected = true; }
    },
    requestAnimationFrame: callback => frames.push(callback),
    setTimeout: (callback, delay) => { timers.set(++nextTimer, {callback, delay}); return nextTimer; },
    clearTimeout: id => timers.delete(id),
  };
  runInNewContext(source, context);
  return {calls, frames, timers, nodes,
    mutate: () => { if (!disconnected) mutation(); },
    frame: () => frames.shift()(),
    snapshot: () => { for (const [id, timer] of [...timers]) { timers.delete(id); timer.callback(); } },
    ready: () => { nodes.set(".time-pill", {}); nodes.set(".emoji-button", {}); nodes.set(".button-row", {textContent: "private workspace title"}); },
    phase: () => JSON.parse(calls.filter(x => x.stage === "boot").at(-1).message).phase,
  };
}
const flush = async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); };

test("missing DOM cannot pass; only three bounded, content-free snapshots", () => {
  const h = harness();
  for (let i = 0; i < 100; i++) h.mutate();
  assert.deepEqual([...h.timers.values()].map(x => x.delay), [5000, 20000, 45000]);
  h.snapshot();
  assert.equal(h.phase(), "waiting-dom");
  assert.equal(h.calls.length, 4);
  assert.equal(h.frames.length, 0);
  h.snapshot();
  assert.equal(h.calls.length, 4);
});
test("loading placeholder retains original gate", () => {
  const h = harness(); h.ready(); h.nodes.get(".button-row").textContent = "Loading...";
  h.mutate(); h.snapshot();
  assert.equal(h.phase(), "waiting-dom");
  assert.equal(h.frames.length, 0);
});
for (const count of [0, 1]) {
  test(`stalled frame ${count + 1} is distinguishable without passing gate`, () => {
    const h = harness(); h.ready(); h.mutate();
    for (let i = 0; i < count; i++) h.frame();
    h.snapshot();
    assert.equal(h.phase(), `waiting-frame-${count + 1}`);
    assert.equal(h.calls.some(x => x.stage === "rendered"), false);
    assert.equal(JSON.stringify(h.calls).includes("private workspace"), false);
  });
}
test("render acknowledgement requires both frames and cancels diagnostic timers", async () => {
  const h = harness(); h.ready(); h.mutate(); h.frame();
  assert.equal(h.calls.some(x => x.stage === "rendered"), false);
  h.frame(); await flush();
  assert.equal(h.calls.filter(x => x.stage === "rendered").length, 1);
  assert.equal(h.timers.size, 0);
  h.mutate(); assert.equal(h.frames.length, 0);
});
for (const pendingRender of [false, true]) {
  test(`render IPC ${pendingRender ? "pending" : "rejected"} stays diagnostic, no retry`, async () => {
    const h = harness({rejectRender: true, pendingRender}); h.ready(); h.mutate(); h.frame(); h.frame();
    await flush(); h.snapshot(); await flush();
    assert.equal(h.phase(), pendingRender ? "render-ipc-pending" : "render-ipc-failed");
    assert.equal(h.calls.filter(x => x.stage === "rendered").length, 1);
    assert.equal(h.calls.filter(x => x.stage === "render-report-rejected").length, pendingRender ? 0 : 1);
    assert.equal(h.timers.size, 0);
  });
}
