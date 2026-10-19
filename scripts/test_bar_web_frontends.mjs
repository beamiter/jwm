#!/usr/bin/env node
// Exercise the actual production bundles with an in-memory Tauri IPC host.
// Build React, Solid, Svelte and Vue first (`npm run build` in each bar).
// Requires Playwright; supports the same PLAYWRIGHT_MODULE/CHROME_BIN as
// test_bar_layout.mjs. No display server, WM, or network service is needed.
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = path => readFile(resolve(root, path), 'utf8');
const supportedFrameworks = ['react', 'solid', 'svelte', 'vue'];
const frameworks = process.env.BAR_FRONTENDS?.split(',') || supportedFrameworks;
assert(frameworks.length > 0 && frameworks.every(name => supportedFrameworks.includes(name)),
  'BAR_FRONTENDS must name one or more of react,solid,svelte,vue');
const [layout, installer] = await Promise.all([
  read('crates/xbar_core/src/web_layout.css'), read('crates/xbar_core/src/web_layout.js'),
]);
const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || undefined });
let cases = 0;
try {
  for (const framework of frameworks) {
    const assets = `bars/tauri_${framework}_bar/dist/assets`;
    const names = await readdir(resolve(root, assets));
    const script = await read(`${assets}/${names.find(name => /^index-.*\.js$/.test(name))}`);
    const css = await read(`${assets}/${names.find(name => /^index-.*\.css$/.test(name))}`);
    for (const edge of ['top', 'bottom', 'left', 'right']) {
      const vertical = edge === 'left' || edge === 'right';
      const page = await browser.newPage({ viewport: vertical ? { width: 42, height: 450 } : { width: 900, height: 42 }, reducedMotion: 'reduce' });
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      // Match the actual entry document. Svelte appends into #app, so an
      // extra, full-height #root would incorrectly push its bar offscreen.
      const mountId = framework === 'react' || framework === 'solid' ? 'root' : 'app';
      await page.setContent(`<style>${css}</style><style>${layout}</style><div id="${mountId}"></div>`);
      await page.evaluate(`(function(){const edge='${edge}';const thickness=42;${installer}})()`);
      await page.evaluate(vertical => {
        const callbacks = new Map();
        const listeners = new Map();
        let id = 0;
        let revision = 0;
        window.__barRequests = [];
        const snapshot = {
          wm_available: true, wm_sequence: 7, wm_session_id: 11,
          geometry: { x: -900, y: 80, width: vertical ? 42 : 900, height: vertical ? 450 : 800 },
          tags: Array.from({ length: 9 }, (_, index) => ({ selected: index === 2 || index === 7, occupied: index % 2 === 0, urgent: index === 7, filled: false })),
          active_tag: 7,
          monitor: 0, layout_symbol: '[]=', client_name: 'Browser regression', time: '18:42:09',
          show_seconds: true, layout_selector_open: false,
          audio_device: { name: 'Test speakers', volume: 50, is_muted: false },
          system_details: { cpu_average: 24, memory_total: 8000000000, memory_used: 4000000000, memory_usage_percent: 50 },
          brightness: { percent: 75 }, battery: { percent: 73, charging: false, present: true },
          minimized_windows: Array.from({ length: 3 }, (_, index) => ({ token: index + 1, monitor: 0, title: `Window ${index + 1}`, app_id: 'test', flags: 1 })),
          minimized_overflow: false,
        };
        const publish = () => {
          revision++;
          for (const [handler, event] of listeners) {
            if (event === 'xbar-state') callbacks.get(handler)?.({ event, id: handler, payload: {
              revision, changes: 0xffffffff, partition_changes: 0xffffffff, snapshot: structuredClone(snapshot),
            } });
          }
        };
        window.__barPublish = patch => { Object.assign(snapshot, patch); publish(); };
        window.__TAURI_INTERNALS__ = {
          metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main', windowLabel: 'main' } },
          transformCallback(callback) { callbacks.set(++id, callback); return id; },
          unregisterCallback(id) { callbacks.delete(id); },
          async invoke(command, args) {
            if (command === 'plugin:event|listen') { listeners.set(args.handler, args.event); return args.handler; }
            if (command === 'plugin:event|unlisten') { listeners.delete(args.id); return; }
            if (command === 'plugin:window|scale_factor') return 1;
            if (command === 'plugin:window|inner_position') return { x: -900, y: 80 };
            if (command === 'frontend_ready') {
              publish();
              return;
            }
            if (command === 'dispatch_action') { window.__barRequests.push(args.request); return; }
            throw new Error(`Unexpected IPC command: ${command}`);
          },
        };
        window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener(_event, id) { listeners.delete(id); callbacks.delete(id); } };
      }, vertical);
      await page.addScriptTag({ type: 'module', content: script });
      await page.waitForSelector('.time-pill', { timeout: 5000 });
      await page.waitForFunction(() => document.querySelector('.button-row')?.getAttribute('role') === 'toolbar');
      await page.waitForFunction(() => document.querySelector('.emoji-button[aria-current="true"]')?.tabIndex === 0);
      const description = `${framework}/${edge}`;
      assert.equal(await page.locator('.emoji-button').nth(7).getAttribute('aria-pressed'), 'true', `${description}: urgent workspace retains actual selected semantics`);
      assert.equal(await page.locator('.emoji-button').nth(7).getAttribute('aria-current'), 'true', `${description}: active workspace is exposed`);
      assert.equal(await page.locator('.emoji-button').nth(7).evaluate(node => node.tabIndex), 0, `${description}: active workspace is the initial toolbar entry`);
      await page.locator('.emoji-button').first().focus();
      await page.keyboard.press('Enter');
      assert.equal(await page.evaluate(() => window.__barRequests.filter(request => request.action === 'view_tag_on').length), 1,
        `${description}: workspace keyboard activation dispatches exactly once`);
      // Exercise status/layout controls too: an existing local key handler
      // must never run an action again through the shared document adapter.
      for (const [selector, action] of [
        ['.layout-toggle', 'toggle_layout_selector'],
        ['.brightness-pill', 'adjust_brightness'],
        ['.volume-pill', 'toggle_mute'],
        ['.time-pill', 'toggle_seconds'],
        ['.shell-pill', 'open_shell_hub'],
      ]) {
        for (const key of ['Enter', 'Space']) {
          const before = await page.evaluate(action => window.__barRequests.filter(request => request.action === action).length, action);
          await page.locator(selector).first().focus();
          await page.keyboard.press(key);
          assert.equal(await page.evaluate(action => window.__barRequests.filter(request => request.action === action).length, action), before + 1,
            `${description}: ${selector} ${key} dispatches exactly once`);
        }
      }
      if (vertical) {
        await page.locator('.time-pill').focus();
        await page.keyboard.press('ArrowDown');
        assert.equal(await page.evaluate(() => document.activeElement.classList.contains('volume-pill')), true,
          `${description}: live toolbar follows visual order`);
      }
      for (const [selector, action] of [['.volume-pill', 'adjust_volume'], ['.brightness-pill', 'adjust_brightness']]) {
        await page.locator(selector).focus();
        for (const key of ['PageUp', 'PageDown', '+', '-']) {
          const before = await page.evaluate(action => window.__barRequests.filter(request => request.action === action).length, action);
          await page.keyboard.press(key);
          const adjusted = await page.evaluate(action => window.__barRequests.filter(request => request.action === action), action);
          assert.equal(adjusted.length, before + 1, `${description}: ${selector} ${key} dispatches exactly once`);
          assert.equal(Math.sign(adjusted.at(-1).delta), key === 'PageUp' || key === '+' ? 1 : -1, `${description}: ${selector} ${key} direction`);
        }
      }
      await page.locator('[data-window-id="2"]').focus();
      await page.evaluate(() => window.__barPublish({ wm_sequence: 8, time: '18:42:10' }));
      await page.waitForFunction(() => document.activeElement.dataset.windowId === '2' &&
        window.__barRequests.some(request => request.action === 'set_dock_geometry' && request.minimized_generation === 8));
      assert.equal(await page.evaluate(() => [...document.querySelectorAll('.button-row button, .button-row [role="button"]')].filter(node => node.tabIndex === 0).length), 1,
        `${description}: generation replacement preserves a single focused toolbar control`);
      await page.evaluate(() => window.__barPublish({ layout_selector_open: true }));
      await page.waitForFunction(() => document.querySelector('.layout-option')?.hasAttribute('tabindex') && document.querySelector('.layout-toggle')?.getAttribute('aria-expanded') === 'true');
      await page.locator('.layout-option').first().focus();
      const togglesBeforeEscape = await page.evaluate(() => window.__barRequests.filter(request => request.action === 'toggle_layout_selector').length);
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => window.__barRequests.filter(request => request.action === 'toggle_layout_selector').length), togglesBeforeEscape + 1,
        `${description}: Escape uses the frontend's layout toggle action`);
      await page.evaluate(() => window.__barPublish({ layout_selector_open: false }));
      await page.waitForFunction(() => !document.querySelector('.layout-option') && document.activeElement.classList.contains('layout-toggle'));
      await page.waitForFunction(() => window.__barRequests.some(request => request.action === 'set_dock_geometry' && request.window_id === 3));
      const previous = await page.evaluate(() => window.__barRequests.filter(request => request.action === 'set_dock_geometry').length);
      await page.locator('.right-info-container').evaluate((node, vertical) => vertical ? node.scrollTo(0, node.scrollHeight) : node.scrollTo(node.scrollWidth, 0), vertical);
      await page.waitForFunction(previous => window.__barRequests.filter(request => request.action === 'set_dock_geometry').length > previous, previous);
      await page.waitForFunction(() => {
        return [...document.querySelectorAll('[data-window-id]')].every(item => {
          const rect = window.__xbarLayout.restingDockRect(item, item.closest('.minimized-dock'));
          const expected = rect ? { x: -900 + Math.round(rect.left), y: 80 + Math.round(rect.top), width: Math.round(rect.width), height: Math.round(rect.height) }
            : { x: -900, y: 80, width: 0, height: 0 };
          const actual = window.__barRequests.filter(request => request.action === 'set_dock_geometry' && request.window_id === Number(item.dataset.windowId)).at(-1)?.geometry;
          return actual && Object.keys(expected).every(key => actual[key] === expected[key]);
        });
      }, null, { timeout: 5000 });
      await page.locator('[data-window-id="3"]').focus();
      await page.keyboard.press('Enter');
      await page.waitForFunction(() => window.__barRequests.some(request => request.action === 'restore_window'));
      const restored = await page.evaluate(() => window.__barRequests.filter(request => request.action === 'restore_window'));
      assert.equal(restored.length, 1, `${description}: one keyboard restore`);
      assert.equal(restored[0].window_id, 3, description);
      assert(restored[0].geometry.width > 0 && restored[0].geometry.height > 0, `${description}: restore uses visible geometry`);
      assert.deepEqual(errors, [], description);
      await page.close();
      cases++;
    }
  }
} finally {
  await browser.close();
}
console.log(`PASS: ${cases} production-bundle cases; workspace semantics, keyboard actions/adjustments, reactive focus preservation, Escape, scroll-triggered anchor publication, clipped withdrawal, and restore.`);
