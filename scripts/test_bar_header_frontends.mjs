#!/usr/bin/env node
// Load the six real frontend production bundles with a private in-memory
// Tauri bridge. No WM, desktop session, or real IPC command is contacted.
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { dirname, resolve, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const supported = ['react', 'solid', 'svelte', 'vue', 'leptos', 'yew'];
const frameworks = process.env.BAR_FRONTENDS?.split(',') || supported;
assert(frameworks.length && frameworks.every(name => supported.includes(name)));
const [css, installer] = await Promise.all(['css', 'js'].map(ext => readFile(resolve(root, `crates/xbar_core/src/web_layout.${ext}`), 'utf8')));
const nativeScroll = await readFile(resolve(root, 'crates/xbar_core/crates/xbar_tauri/src/native_scroll.js'), 'utf8');
const mime = { '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm', '.html': 'text/html', '.woff2': 'font/woff2' };
const server = createServer(async (req, res) => {
  try {
    if (req.url === '/favicon.ico') { res.writeHead(204).end(); return; }
    const path = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    const [, framework, ...parts] = path.split('/');
    assert(supported.includes(framework));
    const base = resolve(root, `bars/tauri_${framework}_bar/dist`);
    const file = resolve(base, parts.join('/') || 'index.html');
    assert(file.startsWith(`${base}/`));
    res.setHeader('Content-Type', mime[extname(file)] || 'application/octet-stream');
    const contents = await readFile(file);
    res.end(extname(file) === '.html' ? contents.toString().replaceAll('src="/', `src="/${framework}/`).replaceAll('href="/', `href="/${framework}/`).replaceAll("from '/", `from '/${framework}/`).replaceAll("module_or_path: '/", `module_or_path: '/${framework}/`) : contents);
  } catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || undefined, args: ['--disable-gpu'] });
const results = [];
try {
  for (const framework of frameworks) {
    const page = await browser.newPage({ viewport: { width: 1280, height: 42 }, reducedMotion: 'reduce' });
    // Inject the test packet at trusted DOM delivery, just before the native
    // bridge. CDP's input transport may take seconds; GTK's observed callback
    // and WebKit DOM delivery are adjacent and separately verified natively.
    await page.addInitScript({ content: `window.__testNativePackets = []; window.addEventListener('wheel', event => { if (event.isTrusted) { for (const packet of window.__testNativePackets.splice(0)) { const { nativeTimeOffset = 0, ...fields } = packet; window.__xbarNativeScroll.record({ ...fields, nativeTime: (Math.round(event.timeStamp) + nativeTimeOffset) >>> 0 }); } } }, { capture: true });\n${nativeScroll}` });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    await page.addInitScript(() => {
      let revision = 0, nextId = 0;
      const callbacks = new Map(), listeners = new Map(), rustListeners = [];
      window.__barRequests = [];
      const state = { selected: false, occupied: false, urgent: false, filled: false };
      const workspace = (id, selected = false, occupied = true, filled = false) => ({ id, number: id + 1, state: { ...state, selected, occupied, filled } });
      let projection = [workspace(0, false, true, true), workspace(2), workspace(4, true, false), workspace(7)];
      const snapshot = {
        wm_available: true, wm_sequence: 7, wm_session_id: 11, theme: 'Light',
        geometry: { x: -1280, y: 80, width: 1280, height: 42 },
        tags: Array.from({ length: 9 }, (_, id) => ({ ...state, selected: id === 4, occupied: [0, 2, 7].includes(id), filled: id === 0 })),
        active_tag: 8, monitor: 3, layout_symbol: '[]=', client_name: 'Browser header regression',
        time: '2026-10-29 13:07:43', show_seconds: true, layout_selector_open: false,
        audio_device: { name: 'Test speakers', volume: 50, is_muted: false },
        system_details: { cpu_average: 24, memory_total: 8000000000, memory_used: 4000000000, memory_usage_percent: 50 },
        brightness: { percent: 75 }, battery: { percent: 73, charging: false, present: true },
        minimized_windows: [{ token: 1, monitor: 3, title: 'Hidden duplicate Dock', app_id: 'test', flags: 1 }], minimized_overflow: false,
      };
      const publish = () => {
        const payload = { revision: ++revision, changes: 0xffffffff, partition_changes: 0xff, snapshot: structuredClone(snapshot), workspace_tags: projection,
          header_cpu: '24%', header_memory: '50%', header_network: '↓1.0KiB/s ↑2.0KiB/s', header_clock_compact: '13:07:43' };
        for (const [id, event] of listeners) if (event === 'xbar-state') callbacks.get(id)?.({ event, id, payload });
        for (const callback of rustListeners) callback({ payload });
      };
      window.__barPublish = (patch = {}, tags) => { Object.assign(snapshot, patch); if (tags) projection = tags; publish(); };
      const invoke = async (command, args) => {
        if (command === 'plugin:event|listen') { listeners.set(args.handler, args.event); return args.handler; }
        if (command === 'plugin:event|unlisten') { listeners.delete(args.id); return; }
        if (command === 'plugin:window|scale_factor') return 1;
        if (command === 'plugin:window|inner_position') return { x: -1280, y: 80 };
        if (command === 'frontend_ready') { publish(); return; }
        if (command === 'frontend_rendered') return;
        if (command === 'dispatch_action') { window.__barRequests.push(args.request); return; }
        throw new Error(`Unexpected IPC command ${command}`);
      };
      window.__TAURI_INTERNALS__ = { metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main', windowLabel: 'main' } },
        transformCallback(callback) { callbacks.set(++nextId, callback); return nextId; }, unregisterCallback(id) { callbacks.delete(id); }, invoke };
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener(_event, id) { listeners.delete(id); callbacks.delete(id); } };
      window.__TAURI__ = { core: { invoke }, event: { async listen(_event, callback) { rustListeners.push(callback); return () => {}; } },
        window: { getCurrentWindow() { return { async scaleFactor() { return 1; }, async innerPosition() { return { x: -1280, y: 80 }; } }; } } };
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/${framework}/`);
    await page.addStyleTag({ content: css });
    await page.evaluate(`(function(){const edge='top';const thickness=42;${installer}})()`);
    try { await page.waitForSelector('.emoji-button[data-tag-index="4"]', { timeout: 15000 }); } catch (error) { console.error(framework, errors, await page.locator('body').innerText()); throw error; }
    await page.waitForFunction(() => document.querySelector('.button-row')?.getAttribute('role') === 'toolbar');
    await page.evaluate(() => document.fonts.ready);
    const readTags = () => page.locator('.buttons-container > .emoji-button').evaluateAll(nodes => nodes.map(node => ({
      id: Number(node.dataset.tagIndex), label: node.textContent.trim(), selected: node.getAttribute('aria-pressed') === 'true', className: node.className,
    })));
    let tags = await readTags();
    assert.deepEqual(tags.map(t => t.label), ['1', '3', '5', '8'], `${framework}: original sparse numeric labels`);
    assert.deepEqual(tags.filter(t => t.selected).map(t => t.id), [4], `${framework}: empty actual selected retained despite stale global active`);
    assert(!tags[0].className.includes('state-selected'), `${framework}: focused/filled tag is not selected`);
    await page.locator('.emoji-button[data-tag-index="2"]').click();
    assert.deepEqual(await page.evaluate(() => window.__barRequests.filter(r => r.action === 'view_tag_on').at(-1)),
      { action: 'view_tag_on', tag_index: 2, monitor_id: 3 }, `${framework}: original tag and current monitor action`);
    await page.evaluate(() => document.addEventListener('contextmenu', event => { window.__lastContextEvent = event; }, { capture: true, once: true }));
    await page.locator('.emoji-button[data-tag-index="7"]').click({ button: 'right' });
    assert.deepEqual(await page.evaluate(() => window.__barRequests.filter(r => r.action === 'toggle_tag_on').at(-1)),
      { action: 'toggle_tag_on', tag_index: 7, monitor_id: 3 }, `${framework}: right click toggles original tag on this monitor`);
    assert(await page.evaluate(() => window.__lastContextEvent.defaultPrevented), `${framework}: no browser context menu`);
    const wheel = async (delta, expected, modifiers = {}) => {
      await page.waitForTimeout(270); // Start a distinct native-compatible idle-delimited gesture.
      const before = await page.evaluate(() => window.__barRequests.length);
      const prevented = await page.locator('.emoji-button[data-tag-index="2"]').evaluate((node, { delta, modifiers }) => {
        const event = new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY: delta, ...modifiers });
        node.dispatchEvent(event);
        return event.defaultPrevented;
      }, { delta, modifiers });
      assert(prevented, `${framework}: tag wheel prevents duplicate container scroll`);
      if (expected === null) {
        await page.waitForTimeout(40);
        assert.equal(await page.evaluate(() => window.__barRequests.length), before, `${framework}: modified/zero wheel ignored`);
      } else {
        await page.waitForFunction(before => window.__barRequests.length > before, before);
        const requests = await page.evaluate(before => window.__barRequests.slice(before), before);
        assert.deepEqual(requests, [{ action: 'view_tag_on', tag_index: expected, monitor_id: 3 }], `${framework}: one normalized eligible wheel action`);
      }
    };
    // Raw active=8 is stale and unselected: actual selected=5 anchors the
    // eligible 1/3/5/8 sequence, independent of the hovered pill.
    await wheel(120, 7);
    await wheel(-.25, null);
    await wheel(-1, 2, { deltaMode: 1 });
    await wheel(0, null);
    await wheel(120, null, { ctrlKey: true });
    await page.waitForTimeout(270);
    const precisionBefore = await page.evaluate(() => window.__barRequests.length);
    await page.locator('.emoji-button[data-tag-index="2"]').evaluate(node => {
      for (const deltaY of [.25, 19.75, 20, 400]) node.dispatchEvent(new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY }));
    });
    await page.waitForFunction(before => window.__barRequests.length > before, precisionBefore);
    assert.deepEqual(await page.evaluate(before => window.__barRequests.slice(before), precisionBefore),
      [{ action: 'view_tag_on', tag_index: 7, monitor_id: 3 }], `${framework}: 40px threshold and one action for a precision swipe including momentum`);
    // The native GTK bridge retains the direction/source that WebKitGTK
    // loses: a notch and smooth delta 1.0 both reach DOM as 21 pixels / -120.
    // Real trusted browser events exercise the bridge; synthetic pixel events
    // above continue to exercise the unchanged 40px precision policy directly.
    const targetBox = await page.locator('.emoji-button[data-tag-index="2"]').boundingBox();
    const pointer = { x: targetBox.x + targetBox.width / 2, y: targetBox.y + targetBox.height / 2 };
    const recordNative = async (kind, dy, extra = {}) => {
      const { immediate = false, ...fields } = extra;
      await page.evaluate(({ packet, immediate }) => {
        if (immediate) window.__xbarNativeScroll.record({ ...packet, nativeTime: Math.round(performance.now()) >>> 0 });
        else window.__testNativePackets.push(packet);
      }, { packet: { kind, dx: 0, dy, ...pointer, ...fields }, immediate });
    };
    const requestsSince = before => page.evaluate(before => window.__barRequests.slice(before), before);
    const nextGesture = async () => { await page.waitForTimeout(270); await page.mouse.move(pointer.x, pointer.y); return page.evaluate(() => window.__barRequests.length); };
    let beforeNative = await nextGesture();
    await recordNative('line', 1);
    await page.mouse.wheel(0, 21);
    await page.waitForFunction(before => window.__barRequests.length > before, beforeNative);
    assert.deepEqual(await requestsSince(beforeNative), [{ action: 'view_tag_on', tag_index: 7, monitor_id: 3 }], `${framework}: native notch becomes exactly one original-ID action`);
    beforeNative = await nextGesture();
    await recordNative('line', 1, { nativeTimeOffset: -20 }); // An older unmatched packet cannot classify a new precise event.
    await recordNative('pixel', 1);
    await page.mouse.wheel(0, 21);
    await page.waitForTimeout(40);
    assert.deepEqual(await requestsSince(beforeNative), [], `${framework}: precise source cannot inherit an older notch`);
    await recordNative('pixel', 2);
    await page.mouse.wheel(0, 42);
    await page.waitForFunction(before => window.__barRequests.length > before, beforeNative);
    assert.deepEqual(await requestsSince(beforeNative), [{ action: 'view_tag_on', tag_index: 7, monitor_id: 3 }], `${framework}: native precise source retains the 40px threshold`);
    for (const boundary of ['stop', 'mismatch', 'expired']) {
      beforeNative = await nextGesture();
      await recordNative('line', 1, boundary === 'mismatch' ? { x: pointer.x + 100 } : boundary === 'expired' ? { immediate: true } : { nativeTimeOffset: -20 });
      if (boundary === 'stop') await recordNative('pixel', 0, { stop: true, nativeTimeOffset: -5 });
      if (boundary === 'expired') await page.waitForTimeout(230);
      await page.mouse.wheel(0, 21);
      await page.waitForTimeout(40);
      assert.deepEqual(await requestsSince(beforeNative), [], `${framework}: ${boundary} native packet cannot activate a workspace`);
    }
    await page.setViewportSize({ width: 1280, height: 200 });
    await page.evaluate(() => {
      const area = document.createElement('div');
      area.id = 'native-scroll-default';
      area.style.cssText = 'position:absolute;left:0;top:60px;width:250px;height:100px;overflow:auto;';
      area.innerHTML = '<div style="height:1000px">Native default scrolling</div>';
      document.body.appendChild(area);
      window.__testNativePackets.push({ kind: 'line', x: 50, y: 100, dx: 0, dy: 1 });
    });
    await page.mouse.move(50, 100);
    await page.mouse.wheel(0, 21);
    await page.waitForFunction(() => document.querySelector('#native-scroll-default').scrollTop > 0);
    await page.evaluate(() => document.querySelector('#native-scroll-default').remove());
    await page.setViewportSize({ width: 1280, height: 42 });
    const beforeKeyboard = await page.evaluate(() => window.__barRequests.length);
    await page.locator('.emoji-button[data-tag-index="2"]').focus();
    await page.keyboard.press('ArrowRight');
    assert.equal(await page.evaluate(() => document.activeElement.dataset.tagIndex), '4', `${framework}: arrow moves focus without changing workspace`);
    assert.deepEqual(await requestsSince(beforeKeyboard), []);
    await page.keyboard.press('Enter');
    await page.waitForFunction(before => window.__barRequests.length > before, beforeKeyboard);
    assert.deepEqual(await requestsSince(beforeKeyboard), [{ action: 'view_tag_on', tag_index: 4, monitor_id: 3 }]);
    const beforeMetricKeys = await page.evaluate(() => window.__barRequests.length);
    await page.locator('.volume-pill').focus();
    for (const key of ['PageUp', 'PageDown', '+', '-']) await page.keyboard.press(key);
    await page.waitForFunction(before => window.__barRequests.length >= before + 4, beforeMetricKeys);
    assert.deepEqual(await requestsSince(beforeMetricKeys), [5, -5, 5, -5].map(delta => ({ action: 'adjust_volume', delta })), `${framework}: existing metric keyboard steps are preserved`);
    const projection = ids => ids.map(id => ({ id, number: id + 1, state: { selected: [4, 7].includes(id), occupied: [0, 2, 7].includes(id), filled: false, urgent: id === 7 } }));
    await page.evaluate(tags => window.__barPublish({}, tags), projection([0, 2, 4, 7]));
    await page.waitForFunction(() => document.querySelectorAll('.emoji-button[aria-pressed="true"]').length === 2);
    assert.deepEqual((await readTags()).filter(t => t.selected).map(t => t.id), [4, 7], `${framework}: multi-tag selection survives urgency`);
    await page.evaluate(() => window.__barPublish({ active_tag: 7 }));
    await page.waitForFunction(() => document.querySelector('.button-row').dataset.activeTag === '7');
    await wheel(1, 0, { deltaMode: 1 });
    await wheel(-1, 4, { deltaMode: 2 });
    const layouts = [];
    for (const width of [1600, 1280, 480]) {
      await page.setViewportSize({ width, height: 42 });
      await page.waitForTimeout(80);
      const geometry = await page.evaluate(() => {
        const bar = document.querySelector('.button-row'), leading = bar.querySelector('.buttons-container'), status = bar.querySelector('.right-info-container');
        const rect = node => { const r = node.getBoundingClientRect(); return { x: r.x, right: r.right, width: r.width, height: r.height }; };
        const tags = [...leading.querySelectorAll('.emoji-button')].map(node => ({ ...rect(node), selected: node.getAttribute('aria-pressed') === 'true', font: getComputedStyle(node).fontSize }));
        const statuses = [...status.querySelectorAll('.pill')].filter(node => node.getClientRects().length).map(node => ({ className: node.className, order: +getComputedStyle(node).order, ...rect(node), fill: getComputedStyle(node).backgroundColor })).sort((a,b) => a.x-b.x);
        return { bar: rect(bar), leading: rect(leading), status: rect(status), tags, statuses,
          title: bar.querySelector('.client-title').textContent, launcher: bar.querySelector('.header-launcher-glyph').textContent,
          duplicates: [...bar.querySelectorAll('.minimized-dock,.brightness-pill,.layout-controls')].filter(node => node.getClientRects().length).length,
          clock: getComputedStyle(bar.querySelector('.time-pill'), '::after').content,
          requests: window.__barRequests };
      });
      assert.equal(geometry.launcher, '▦', `${framework}: Tao launcher`);
      assert.equal(geometry.title, 'Browser header regression');
      assert.equal(geometry.duplicates, 0, `${framework}: no duplicate companion controls`);
      for (const tag of geometry.tags) {
        assert(tag.width >= 32 && tag.width <= 40, `${framework}: compact numeric tag ${tag.width}`);
        assert.equal(tag.font, '12px');
      }
      assert(geometry.status.width <= width * .49 + 1, `${framework}: native status budget`);
      assert(geometry.statuses.every(s => s.fill === 'rgba(0, 0, 0, 0)'), `${framework}: flat native status style`);
      assert.deepEqual(geometry.statuses.map(s => s.order), [...geometry.statuses.map(s => s.order)].sort((a,b) => a-b), `${framework}: visible DOM order follows Tao`);
      assert(!geometry.requests.some(r => r.action === 'set_dock_geometry'), `${framework}: top never writes companion Dock anchors`);
      if (width < 1424) assert.equal(geometry.clock, '"13:07:43"', `${framework}: canonical compact clock`);
      layouts.push({ width, ...geometry });
      if (process.env.BAR_HEADER_SHOTS && width === 1280) { await mkdir(resolve(process.env.BAR_HEADER_SHOTS), { recursive: true }); await page.screenshot({ path: resolve(process.env.BAR_HEADER_SHOTS, `${framework}-1280-light.png`) }); }
    }
    await page.evaluate(script => new Function('edge', 'thickness', 'configuredScroll', script)('top', 42,
      { navigation_pixels: 60, adjustment_pixels: 16, idle_ms: 250 }), installer);
    const customBefore = await page.evaluate(() => window.__barRequests.length);
    await page.locator('.emoji-button[data-tag-index="2"]').evaluate(node => {
      for (const deltaY of [20, 20]) node.dispatchEvent(new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY }));
    });
    assert.equal(await page.evaluate(() => window.__barRequests.length), customBefore, `${framework}: configured 60px navigation threshold does not fire at 40px`);
    await page.locator('.emoji-button[data-tag-index="2"]').evaluate(node => {
      for (const deltaY of [20, 400]) node.dispatchEvent(new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY }));
    });
    await page.waitForFunction(before => window.__barRequests.length > before, customBefore);
    assert.deepEqual(await page.evaluate(before => window.__barRequests.slice(before), customBefore),
      [{ action: 'view_tag_on', tag_index: 0, monitor_id: 3 }], `${framework}: configured 60px threshold fires once including momentum`);
    const all = Array.from({ length: 18 }, (_, id) => ({ id, number: id + 1, state: { selected: id === 17, occupied: true, urgent: false, filled: false } }));
    await page.evaluate(tags => window.__barPublish({}, tags), all);
    await page.waitForFunction(() => document.querySelector('.emoji-button[data-tag-index="17"]')?.getAttribute('aria-pressed') === 'true');
    await page.waitForFunction(() => {
      const node=document.querySelector('.emoji-button[data-tag-index="17"]');
      if (!node) return false;
      const r=node.getBoundingClientRect(), g=node.closest('.buttons-container').getBoundingClientRect();
      return r.left >= g.left - 1 && r.right <= g.right + 1;
    }, null, { timeout: 5000 });
    const selectedBox = await page.locator('.emoji-button[data-tag-index="17"]').evaluate(node => {
      const r=node.getBoundingClientRect(), group=node.closest('.buttons-container'), g=group.getBoundingClientRect(); return { left:r.left, right:r.right, groupLeft:g.left, groupRight:g.right, scroll:group.scrollLeft, scrollWidth:group.scrollWidth, clientWidth:group.clientWidth, overflow:getComputedStyle(group).overflowX, focused:document.activeElement?.dataset.tagIndex };
    });
    assert(selectedBox.left >= selectedBox.groupLeft - 1 && selectedBox.right <= selectedBox.groupRight + 1,
      `${framework}: narrow strip brings selected original tag into view ${JSON.stringify({selectedBox,errors})}`);
    await page.locator('.emoji-button[data-tag-index="0"]').focus();
    await page.keyboard.press('Enter');
    assert.equal(await page.evaluate(() => window.__barRequests.filter(r => r.action === 'view_tag_on').at(-1).tag_index), 0, `${framework}: overflow tags remain keyboard accessible`);
    assert.deepEqual(errors, [], `${framework}: no browser exception`);
    results.push({ framework, layouts });
    console.log(`PASS ${framework}: sparse and multi-selected tags, original actions, right click and eligible wheel, three viewport budgets, selected overflow, no top Dock IPC`);
    await page.close();
  }
} finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
if (process.env.BAR_HEADER_RESULTS) { await mkdir(dirname(resolve(process.env.BAR_HEADER_RESULTS)), { recursive: true }); await import('node:fs/promises').then(fs => fs.writeFile(process.env.BAR_HEADER_RESULTS, JSON.stringify(results, null, 2))); }
console.log(`PASS: ${results.length} real frontend bundles, ${results.length * 3} viewport layouts.`);
