#!/usr/bin/env node
// Browser regression for the shared adapter against each frontend's real CSS.
// Requires Playwright. Optional: PLAYWRIGHT_MODULE=/absolute/module/path,
// CHROME_BIN=/path/to/chrome, BAR_SCREENSHOTS=/temporary/output/directory.
import assert from 'node:assert/strict';
import { readFile, mkdir } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = path => readFile(resolve(root, path), 'utf8');
const [layout, installer] = await Promise.all([
  read('crates/xbar_core/src/web_layout.css'),
  read('crates/xbar_core/src/web_layout.js'),
]);
const styles = [
  ['react', 'bars/tauri_react_bar/src/App.css'],
  ['solid', 'bars/tauri_solid_bar/src/App.css'],
  ['svelte', 'bars/tauri_svelte_bar/src/app.css'],
  ['vue', 'bars/tauri_vue_bar/src/App.vue'],
  ['leptos', 'bars/tauri_leptos_bar/src/app.css'],
  ['yew', 'bars/tauri_yew_bar/src/app.css'],
  ['dioxus', 'bars/dioxus_bar/assets/style.css'],
];

// Runs in the page, against live layout boxes rather than stylesheet text.
// Keep this function self-contained so it can also run in a standalone
// Chromium fixture when Playwright is unavailable in a constrained runner.
function metricStabilityProbe(vertical) {
  const metrics = [...document.querySelectorAll('.usage-pill, .brightness-pill, .volume-pill')];
  const tracked = [...document.querySelectorAll('.button-row .pill, .system-info-container, .minimized-dock')];
  const originals = metrics.map(node => ({
    html: node.innerHTML, font: node.style.fontFamily, className: node.className,
  }));
  const rect = node => {
    const value = node.getBoundingClientRect();
    return { x: value.x, y: value.y, width: value.width, height: value.height };
  };
  const read = () => tracked.map(rect);
  const valueRect = node => {
    const texts = [...node.childNodes].filter(child => child.nodeType === Node.TEXT_NODE && child.textContent.trim());
    const first = texts[0];
    const last = texts[texts.length - 1];
    const range = document.createRange();
    range.setStart(first, first.textContent.search(/\S/));
    range.setEnd(last, last.textContent.trimEnd().length);
    return range.getBoundingClientRect();
  };
  const change = (node, value, icon) => {
    const glyph = node.querySelector('.nf-icon');
    for (const child of [...node.childNodes]) if (child !== glyph) child.remove();
    glyph.textContent = icon;
    // React/Solid may own the number and suffix as separate text nodes;
    // reactive framework comment anchors must not create another grid row.
    node.append(document.createTextNode(' '), document.createComment('metric-start'));
    if (value.endsWith('%')) {
      node.append(document.createTextNode(value.slice(0, -1)),
        document.createComment('metric-suffix'), document.createTextNode('%'));
    } else {
      node.append(document.createTextNode(value));
    }
    node.classList.toggle('muted', node.classList.contains('volume-pill') && value === '--');
  };
  let cases = 0;
  try {
    for (const font of ['', 'Georgia, serif']) {
      metrics.forEach((node, index) => {
        node.style.fontFamily = font || originals[index].font;
        change(node, '100%', 'W');
      });
      const baseline = read();
      const valueEnds = metrics.map(node => valueRect(node).right);
      for (const [index, metric] of metrics.entries()) {
        for (const [step, value] of ['0%', '9%', '10%', '11%', '88%', '99%', '100%', '--', '--%'].entries()) {
          change(metric, value, step % 2 ? 'W' : 'I');
          const current = read();
          for (const [position, box] of current.entries()) {
            for (const key of ['x', 'y', 'width', 'height']) {
              if (Math.abs(box[key] - baseline[position][key]) > 0.05) {
                throw new Error(`metric ${index} ${value} (${font || 'theme'}): ${tracked[position].className} ${key} moved from ${baseline[position][key]} to ${box[key]}`);
              }
            }
          }
          if (!vertical) {
            const valueBox = valueRect(metric);
            const box = metric.getBoundingClientRect();
            if (Math.abs(valueBox.right - valueEnds[index]) > 0.05) {
              throw new Error(`metric ${index} ${value}: numeric column is not right aligned`);
            }
            // Range boxes include a font's logical ascent/descent; tolerate
            // half-pixel rounding at the 21px high-DPI fixture boundary.
            if (valueBox.left < box.left - 0.6 || valueBox.right > box.right + 0.6 || valueBox.top < box.top - 0.6 || valueBox.bottom > box.bottom + 0.6) {
              throw new Error(`metric ${index} ${value}: reading does not fit the pill (${JSON.stringify(valueBox)} inside ${JSON.stringify(box)})`);
            }
          }
          cases++;
        }
        change(metric, '100%', 'W');
      }
    }
  } finally {
    metrics.forEach((node, index) => {
      node.innerHTML = originals[index].html;
      node.style.fontFamily = originals[index].font;
      node.className = originals[index].className;
    });
  }
  return { metrics: metrics.length, cases };
}

async function assertHoverStability(page, description) {
  const rects = () => [...document.querySelectorAll('.button-row .pill, .system-info-container')]
    .map(node => {
      const { x, y, width, height } = node.getBoundingClientRect();
      return { x, y, width, height };
    });
  for (const selector of ['.volume-pill', '.brightness-pill', '.layout-toggle', '.layout-option']) {
    const target = page.locator(selector).first();
    await target.scrollIntoViewIfNeeded();
    await page.mouse.move(-10, -10);
    const before = await page.evaluate(rects);
    await target.hover();
    assert.equal(await target.evaluate(node => node.matches(':hover')), true, `${description}: actual hover on ${selector}`);
    assert.deepEqual(await page.evaluate(rects), before, `${description}: ${selector} hover must preserve layout and hit rectangles`);
    assert.equal(await target.evaluate(node => getComputedStyle(node).borderLeftWidth), '1px', `${description}: ${selector} border stays constant`);
  }
  await page.mouse.move(-10, -10);
  await page.evaluate(() => document.querySelectorAll('.buttons-container, .right-info-container').forEach(node => node.scrollTo(0, 0)));
}

const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN || undefined,
  headless: true,
});
let cases = 0;
let metricCases = 0;
try {
  for (const [name, path] of styles) {
    const source = await read(path);
    const css = name === 'vue' ? source.match(/<style[^>]*>([\s\S]*?)<\/style>/)[1] : source;
    for (const edge of ['top', 'bottom', 'left', 'right']) {
      for (const [length, thickness] of [[900, 42], [320, 42], [450, 21]]) {
        const vertical = edge === 'left' || edge === 'right';
        const viewport = vertical ? { width: thickness, height: length } : { width: length, height: thickness };
        const page = await browser.newPage({ viewport, reducedMotion: 'reduce' });
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        const tags = Array.from({ length: 9 }, (_, i) =>
          `<button class="emoji-button" title="Workspace ${i + 1}">${i + 1}</button>`).join('');
        const cards = Array.from({ length: 3 }, (_, i) => name === 'dioxus'
          ? `<div class="minimized-dock-slot"><div class="minimized-dock-card" data-window-id="${i + 1}" title="Restore window ${i + 1}">${i + 1}</div></div>`
          : `<button class="minimized-item" data-window-id="${i + 1}" title="Restore window ${i + 1}"><span class="minimized-thumbnail"><span class="minimized-initial">${i + 1}</span></span></button>`).join('');
        const metricTag = name === 'svelte' ? 'button' : 'div';
        await page.setContent(`<!doctype html><html><head><style>${css}</style><style>${layout}</style></head><body><div id="root"><div class="button-row">
          <div class="buttons-container">${tags}<div class="layout-controls"><div class="pill layout-toggle" title="Choose layout">[]=</div><div class="layout-selector"><div class="pill layout-option" title="Tiled">[]=</div><div class="pill layout-option" title="Floating">&gt;&lt;&gt;</div></div></div></div>
          <div class="spacer"></div><div class="right-info-container">
          <div class="minimized-dock"><span class="minimized-divider"></span>${cards}</div>
          <div class="system-info-container"><div class="pill usage-pill"><span class="nf-icon">C</span>82%</div><div class="pill usage-pill"><span class="nf-icon">M</span>43%</div><div class="pill usage-pill"><span class="nf-icon">B</span>73%</div></div>
          <${metricTag} class="pill brightness-pill" title="Brightness"><span class="nf-icon">B</span>75%</${metricTag}>
          <${metricTag} class="pill volume-pill" title="Volume"><span class="nf-icon">V</span>50%</${metricTag}>
          <div class="shell-menu"><div class="pill shell-pill" title="Control center">S</div></div>
          <div class="pill screenshot-pill" title="Screenshot">P</div>
          <div class="pill time-pill"><span class="nf-icon">T</span>18:42:09</div>
          <div class="pill monitor-pill">1</div><div class="pill scale-pill">1.00</div>
          </div></div></div></body></html>`);
        const install = `(function(){const edge='${edge}';const thickness=${thickness};${installer}})()`;
        await page.evaluate(install);
        // Readiness replay must replace listeners, not double activation.
        await page.evaluate(install);
        await page.evaluate(() => {
          window.activations = 0;
          document.querySelectorAll('button, [role="button"]').forEach(node =>
            node.addEventListener('click', () => window.activations++));
        });
        const description = `${name}/${edge}/${length}×${thickness}`;
        const bounds = await page.evaluate(vertical => {
          const rect = node => {
            const r = node.getBoundingClientRect();
            return { x: r.x, y: r.y, width: r.width, height: r.height };
          };
          const items = [...document.querySelectorAll('.emoji-button, .layout-toggle, .layout-option, .pill, [data-window-id]')]
            .filter(node => node.getClientRects().length);
          return {
            bar: rect(document.querySelector('.button-row')),
            bad: items.map(rect).filter(r => vertical
              ? r.x < -0.6 || r.x + r.width > innerWidth + 0.6
              : r.y < -0.6 || r.y + r.height > innerHeight + 0.6),
            clock: rect(document.querySelector('.time-pill')),
            status: rect(document.querySelector('.right-info-container')),
            label: document.querySelector('.time-pill').dataset.sidebarLabel,
            orientation: document.querySelector('.button-row').getAttribute('aria-orientation'),
          };
        }, vertical);
        assert.equal(bounds.bar.width, viewport.width, description);
        assert.equal(bounds.bar.height, viewport.height, description);
        assert.deepEqual(bounds.bad, [], `${description}: controls overflow the short axis`);
        assert.equal(bounds.orientation, vertical ? 'vertical' : 'horizontal', description);
        if (vertical) {
          assert.equal(bounds.label, '18\n42\n09', description);
          assert(bounds.clock.y >= bounds.status.y - 1 && bounds.clock.y + bounds.clock.height <= bounds.status.y + bounds.status.height + 1,
            `${description}: clock must remain initially visible`);
        }
        const stableMetrics = await page.evaluate(metricStabilityProbe, vertical);
        assert.equal(stableMetrics.metrics, 5, `${description}: CPU, memory, battery, brightness and volume coverage`);
        metricCases += stableMetrics.cases;
        await assertHoverStability(page, description);
        const first = page.locator('.emoji-button').first();
        await first.focus();
        await page.keyboard.press('Enter');
        assert.equal(await page.evaluate(() => window.activations), 1, `${description}: native activation`);
        await page.keyboard.press(vertical ? 'ArrowDown' : 'ArrowRight');
        assert.equal(await page.evaluate(() => document.activeElement.title), 'Workspace 2', description);
        await page.locator('.layout-toggle').focus();
        await page.keyboard.press('Space');
        assert.equal(await page.evaluate(() => window.activations), 2, `${description}: legacy activation after replay`);
        if (vertical) {
          await page.locator('.time-pill').focus();
          await page.keyboard.press('ArrowDown');
          assert.equal(await page.evaluate(() => document.activeElement.classList.contains('volume-pill')), true,
            `${description}: keyboard follows the visual status order`);
        }
        await page.locator('.volume-pill').evaluate(node => node.setAttribute('aria-disabled', 'true'));
        await page.locator('.volume-pill').focus();
        await page.keyboard.press('Enter');
        assert.equal(await page.evaluate(() => window.activations), 2, `${description}: disabled activation`);
        await page.keyboard.press('End');
        assert.equal(await page.evaluate(vertical => vertical
          ? document.activeElement.dataset.windowId === '3'
          : document.activeElement.classList.contains('time-pill'), vertical), true, description);
        assert.deepEqual(errors, [], description);
        if (process.env.BAR_SCREENSHOTS && length === 900 && (name === 'react' || name === 'dioxus')) {
          await page.locator('.time-pill').blur();
          await page.evaluate(() => document.querySelectorAll('.buttons-container, .right-info-container').forEach(node => node.scrollTo(0, 0)));
          await mkdir(process.env.BAR_SCREENSHOTS, { recursive: true });
          await page.screenshot({ path: resolve(process.env.BAR_SCREENSHOTS, `${name}-${edge}.png`) });
        }
        await page.close();
        cases++;
      }
    }
  }
  // Geometry is measured after transforms and nested scrolling in actual DOM.
  // This catches stale animation anchors and re-reporting of hidden cards.
  const geometryPage = await browser.newPage({ viewport: { width: 300, height: 240 } });
  await geometryPage.setContent(`<style>*{box-sizing:border-box}body{margin:0}
    #clip{position:absolute;left:50px;top:30px;width:120px;height:100px;overflow:auto;border:2px solid black}
    #content{position:relative;height:500px;width:110px}
    #card{position:absolute;left:20px;top:170px;width:30px;height:20px;transform-origin:50% 100%;transform:translateY(-1px) scale(1.55)}
    </style><div id="clip"><div id="content"><button id="card">A</button></div></div>`);
  await geometryPage.evaluate(`(function(){const edge='left';const thickness=42;${installer}})()`);
  const geometry = await geometryPage.evaluate(() => {
    const card = document.querySelector('#card');
    const clip = document.querySelector('#clip');
    const api = window.__xbarLayout;
    const hidden = api.restingDockRect(card, clip);
    clip.scrollTop = 145;
    const resting = api.restingDockRect(card, clip);
    const visual = api.visibleDockRect(card);
    card.style.transform = 'none';
    const expected = api.visibleDockRect(card);
    clip.scrollTop = 185;
    const partial = api.restingDockRect(card, clip);
    clip.scrollTop = 220;
    const retired = api.restingDockRect(card, clip);
    card.remove();
    const detached = api.visibleDockRect(card);
    return { hidden, resting, visual, expected, partial, retired, detached };
  });
  assert.equal(geometry.hidden, null, 'offscreen card must withdraw its anchor');
  for (const key of ['left', 'top', 'width', 'height']) {
    assert(Math.abs(geometry.resting[key] - geometry.expected[key]) < 0.05, `resting ${key} survives hover and scroll`);
  }
  assert(geometry.visual.width > geometry.resting.width, 'hover uses visual geometry');
  assert(geometry.partial.height > 0 && geometry.partial.height < geometry.resting.height, 'partial clipping is reported accurately');
  assert.equal(geometry.retired, null, 'scrolled-out card withdraws its anchor');
  assert.equal(geometry.detached, null, 'removed card withdraws its anchor');
  await geometryPage.close();

  // Core-positioned Dioxus cards must ignore generic flex/width overrides and
  // remain pinned when the neighboring status scroller moves.
  for (const edge of ['top', 'bottom', 'left', 'right']) {
    const vertical = edge === 'left' || edge === 'right';
    const page = await browser.newPage({ viewport: vertical ? { width: 42, height: 400 } : { width: 400, height: 42 } });
    const css = await read('bars/dioxus_bar/assets/style.css');
    const shelf = vertical ? { x: 0, y: 290, width: 42, height: 104 } : { x: 290, y: 0, width: 104, height: 42 };
    const slot = vertical ? { x: 0, y: 4, width: 42, height: 32 } : { x: 4, y: 0, width: 46, height: 42 };
    await page.setContent(`<style>${css}</style><style>${layout}</style><div class="button-row" style="--xbar-fixed-dock:104px">
      <div class="buttons-container"><button class="emoji-button">1</button></div><div class="spacer"></div>
      <div class="right-info-container"><div style="height:900px;width:900px">Content</div></div>
      <div class="minimized-dock core-dock" style="--dock-left:${shelf.x}px;--dock-top:${shelf.y}px;--dock-width:${shelf.width}px;--dock-height:${shelf.height}px">
        <button class="minimized-dock-slot" style="--slot-left:${slot.x}px;--slot-top:${slot.y}px;--slot-width:${slot.width}px;--slot-height:${slot.height}px">
          <div class="minimized-dock-card" style="--card-left:6px;--card-top:6px;--card-width:30px;--card-height:20px">A</div>
        </button></div></div>`);
    await page.evaluate(`(function(){const edge='${edge}';const thickness=42;${installer}})()`);
    const measured = await page.evaluate(() => {
      const rect = node => {
        const r = node.getBoundingClientRect();
        return { x: r.x, y: r.y, width: r.width, height: r.height };
      };
      const shelf = rect(document.querySelector('.core-dock'));
      const slot = rect(document.querySelector('.minimized-dock-slot'));
      const card = rect(document.querySelector('.minimized-dock-card'));
      document.querySelector('.right-info-container').scrollTo(80, 80);
      return { shelf, slot, card, after: rect(document.querySelector('.minimized-dock-slot')) };
    });
    assert.deepEqual(measured.shelf, shelf, `${edge}: exact core shelf allocation`);
    assert.deepEqual(measured.slot, { x: shelf.x + slot.x, y: shelf.y + slot.y, width: slot.width, height: slot.height }, `${edge}: stable core hit slot`);
    assert.deepEqual(measured.card, { x: shelf.x + slot.x + 6, y: shelf.y + slot.y + 6, width: 30, height: 20 }, `${edge}: exact core visual card`);
    assert.deepEqual(measured.after, measured.slot, `${edge}: status scrolling cannot move core dock`);
    await page.close();
  }
} finally {
  await browser.close();
}
console.log(`PASS: ${cases} browser layout/accessibility cases (7 frontends × 4 edges × 3 sizes), ${metricCases} metric transitions, stable hover targets, transformed/scrolling geometry, and 4 pinned core dock allocations.`);
