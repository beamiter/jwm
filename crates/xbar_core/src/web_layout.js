// `edge` and `thickness` are supplied by the Rust host. This installer can be
// replayed on scale changes/readiness without accumulating global listeners.
const previous = window.__xbarLayout;
if (previous) previous.dispose();
const root = document.documentElement;
root.dataset.barEdge = edge;
root.style.setProperty('--xbar-thickness', `${thickness}px`);
const vertical = edge === 'left' || edge === 'right';
const scrollPolicy = typeof configuredScroll === 'object' ? configuredScroll : { navigation_pixels: 40, adjustment_pixels: 16, idle_ms: 250 };
const legacyButtons = '.emoji-button, .layout-toggle, .layout-option, .volume-pill, .brightness-pill, ' +
  '.shell-pill, .shell-hub-pill, .shell-route, .screenshot-pill, .time-pill, [data-window-id]';
const controlSelector = 'button, [role="button"]';
const scrollGroups = '.buttons-container, .right-info-container';
const editableSelector = 'input, textarea, select, [contenteditable]:not([contenteditable="false"])';
const setAttribute = (node, name, value) => {
  if (node.getAttribute(name) !== value) node.setAttribute(name, value);
};
let frame = 0;
let contentDirty = false;
let entryControl = previous?.entryControl || null;
let entryKey = previous?.entryKey || null;
let entryChosenByUser = previous?.entryChosenByUser || false;
let focusedControl = null;
let focusedKey = null;
let focusedIndex = 0;
let focusOwner = null;
let toolbarHadFocus = false;
let wheelGesture = null;
let metricWheelGesture = null;
let workspaceWheelGesture = null;
let adjustingMetric = false;
let headerTagSignature = null;
const resizeTargets = new Set();
const available = node => {
  if (node.disabled || node.getAttribute('aria-disabled') === 'true' ||
      node.closest('[hidden], [inert], [aria-hidden="true"]') || !node.getClientRects().length) return false;
  if (getComputedStyle(node).visibility !== 'visible') return false;
  // Offscreen content along the scrolling axis is still reachable. Content
  // outside the bar's short axis (such as a clipped legacy hover menu) is not.
  const rect = node.getBoundingClientRect();
  return vertical ? rect.right > 0 && rect.left < innerWidth : rect.bottom > 0 && rect.top < innerHeight;
};
// Follow the displayed flex order, including the priority status group. Using
// screen coordinates would change navigation whenever a group is scrolled.
const orderedControls = bar => {
  const controls = [];
  const visit = node => {
    if (node.matches(controlSelector)) {
      if (available(node)) controls.push(node);
      return;
    }
    const children = [...node.children];
    if (getComputedStyle(node).display.includes('flex')) {
      children.sort((a, b) => Number(getComputedStyle(a).order) - Number(getComputedStyle(b).order));
    }
    children.forEach(visit);
  };
  visit(bar);
  return controls;
};
const controlKey = node => {
  if (node.dataset.windowId) return `window:${node.dataset.windowId}`;
  if (node.matches('.emoji-button')) return `workspace:${node.title}`;
  if (node.matches('.layout-option, .shell-route')) return `option:${node.textContent.trim()}`;
  return ['layout-toggle', 'volume-pill', 'brightness-pill', 'shell-pill', 'shell-hub-pill', 'screenshot-pill', 'time-pill']
    .find(name => node.classList.contains(name)) || null;
};
const tabStops = (bar, entry) => {
  entryControl = entry;
  entryKey = entry ? controlKey(entry) : null;
  for (const node of bar.querySelectorAll(controlSelector)) {
    const index = node === entry ? 0 : -1;
    // A legacy div reports -1 even without a tabindex attribute, but is only
    // programmatically focusable once that attribute is explicitly present.
    setAttribute(node, 'tabindex', String(index));
  }
};
const focusControl = node => {
  if (!node) return;
  node.focus({ preventScroll: true });
  node.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' });
};
const rememberFocus = (bar, node, controls = orderedControls(bar)) => {
  if (!controls.includes(node)) return;
  entryChosenByUser = true;
  focusedControl = node;
  focusedKey = controlKey(node);
  focusedIndex = controls.indexOf(node);
  focusOwner = node.closest('.layout-controls')?.querySelector('.layout-toggle') || null;
  toolbarHadFocus = true;
  tabStops(bar, node);
};
const reading = node => node.textContent.match(/(?:\d{1,3}(?:\.\d+)?\s*%|--%?)/)?.[0] || 'unavailable';
const refreshControls = bar => {
  setAttribute(bar, 'role', 'toolbar');
  setAttribute(bar, 'aria-label', 'JWM status bar');
  setAttribute(bar, 'aria-orientation', vertical ? 'vertical' : 'horizontal');
  for (const target of bar.querySelectorAll(legacyButtons)) {
    if (target.tagName !== 'BUTTON' && !target.hasAttribute('role')) {
      setAttribute(target, 'role', 'button');
    }
    if (target.title && !target.dataset.windowId) {
      setAttribute(target, 'aria-label', target.title);
    }
    if (target.classList.contains('shell-pill-offline')) setAttribute(target, 'aria-disabled', 'true');
    else if (target.matches('.shell-pill, .shell-hub-pill')) target.removeAttribute('aria-disabled');
  }
  for (const icon of bar.querySelectorAll('.nf-icon')) setAttribute(icon, 'aria-hidden', 'true');
  for (const tag of bar.querySelectorAll('.emoji-button')) {
    // The frontend supplies aria-pressed from the actual selected flag:
    // urgency takes precedence in CSS and must not hide selection semantics.
    const description = [tag.classList.contains('state-urgent') && 'Needs attention',
      tag.classList.contains('state-filtered') && 'Filtered workspace',
      tag.classList.contains('has-clients') && 'Contains windows'].filter(Boolean).join('. ');
    setAttribute(tag, 'aria-description', description);
  }
  for (const toggle of bar.querySelectorAll('.layout-toggle')) {
    setAttribute(toggle, 'aria-label', 'Choose window layout');
    setAttribute(toggle, 'aria-expanded', String(toggle.classList.contains('open')));
  }
  for (const option of bar.querySelectorAll('.layout-option')) {
    const label = { '[]=': 'Tiled layout', '><>': 'Floating layout', '[M]': 'Monocle layout' }[option.textContent.trim()];
    setAttribute(option, 'aria-label', option.title || label || 'Window layout');
    setAttribute(option, 'aria-pressed', String(option.classList.contains('current')));
  }
  for (const metric of bar.querySelectorAll('.volume-pill, .brightness-pill')) {
    const volume = metric.classList.contains('volume-pill');
    setAttribute(metric, 'aria-label', volume ? `Volume ${reading(metric)}, toggle mute` : `Brightness ${reading(metric)}, increase brightness`);
    if (volume) setAttribute(metric, 'aria-pressed', String(metric.classList.contains('muted')));
    setAttribute(metric, 'aria-keyshortcuts', 'PageUp PageDown');
    setAttribute(metric, 'aria-description', 'Page Up or plus to increase; Page Down or minus to decrease.');
  }
  for (const clock of bar.querySelectorAll('.time-pill')) {
    const full = clock.textContent.trim();
    const match = full.match(/(\d{1,2}):(\d{2})(?::(\d{2}))?/);
    if (match) {
      setAttribute(clock, 'data-sidebar-label', match.slice(1).filter(Boolean).join('\n'));
      setAttribute(clock, 'aria-label', `Time ${match[0]}`);
      setAttribute(clock, 'aria-description', clock.title || 'Activate to show or hide seconds.');
    }
  }
  const controls = orderedControls(bar);
  const active = document.activeElement;
  if (controls.includes(active)) {
    rememberFocus(bar, active, controls);
  } else {
    const focusLost = toolbarHadFocus && focusedControl && !controls.includes(focusedControl);
    const sameControl = (focusLost ? focusedKey : entryKey) &&
      controls.find(node => controlKey(node) === (focusLost ? focusedKey : entryKey));
    const initialEntry = controls.find(node => node.matches('.emoji-button[aria-current="true"]')) ||
      controls.find(node => node.matches('.emoji-button[aria-pressed="true"], .emoji-button.state-selected')) || controls[0];
    // Framework startup can materialize the toolbar before its first active
    // workspace attribute. Follow that initial state until a user has chosen
    // a focus entry, then preserve their place across unrelated snapshots.
    const next = !entryChosenByUser ? initialEntry :
      controls.includes(entryControl) ? entryControl :
      sameControl ? sameControl :
      focusLost && controls.includes(focusOwner) ? focusOwner :
      focusLost ? controls[Math.min(focusedIndex, controls.length - 1)] :
      initialEntry;
    tabStops(bar, next || null);
    // Restoring a window may transfer native focus away from the bar. Only
    // repair a removed/disabled control while the document still has focus.
    if (focusLost && document.hasFocus() && (active === document.body || active === root || active === focusedControl)) {
      focusControl(next);
    }
  }
  const targets = new Set([bar, ...bar.querySelectorAll(scrollGroups)]);
  for (const target of resizeTargets) if (!targets.has(target)) {
    resizeObserver?.unobserve(target);
    resizeTargets.delete(target);
  }
  for (const target of targets) if (!resizeTargets.has(target)) {
    resizeObserver?.observe(target);
    resizeTargets.add(target);
  }
};
const refreshOverflow = bar => {
  for (const group of bar.querySelectorAll(scrollGroups)) {
    const position = vertical ? group.scrollTop : Math.abs(group.scrollLeft);
    const extent = vertical ? group.scrollHeight - group.clientHeight : group.scrollWidth - group.clientWidth;
    setAttribute(group, 'data-xbar-scroll-before', String(position > 1));
    setAttribute(group, 'data-xbar-scroll-after', String(extent - position > 1));
  }
};
const refresh = () => {
  frame = 0;
  const bar = document.querySelector('.button-row');
  if (bar) {
    layoutDesktopHeader(bar);
    if (contentDirty) refreshControls(bar);
    refreshOverflow(bar);
  }
  contentDirty = false;
};
// Match the native desktop header's measured status budget. Frameworks own
// the text and controls; this adapter only allocates their existing boxes.
const layoutDesktopHeader = bar => {
  if (vertical || bar.dataset.desktopHeader !== 'true') return;
  const style = getComputedStyle(bar);
  const gap = parseFloat(style.columnGap) || 8;
  const available = Math.max(0, bar.clientWidth - (parseFloat(style.paddingLeft) || 0) - (parseFloat(style.paddingRight) || 0));
  const measure = node => {
    if (!node) return 0;
    const previous = node.style.cssText;
    node.style.setProperty('display', node.matches('.usage-pill, .volume-pill') ? 'inline-grid' : 'inline-flex', 'important');
    const width = Math.max(42, node.getBoundingClientRect().width);
    node.style.cssText = previous;
    return width;
  };
  const status = [
    bar.querySelector('.time-pill'),
    bar.querySelector('.system-info-container > .usage-pill:nth-child(3)'),
    bar.querySelector('.volume-pill'),
    bar.querySelector('.system-info-container > .usage-pill:nth-child(1)'),
    bar.querySelector('.system-info-container > .usage-pill:nth-child(2)'),
    bar.querySelector('.network-pill'),
  ];
  let used = 0;
  for (const node of bar.querySelectorAll('.usage-pill, .volume-pill')) {
    const percent = node.textContent.match(/(\d+(?:\.\d+)?)\s*%/);
    node.style.setProperty('--xbar-header-progress', String(percent ? Math.min(100, Number(percent[1])) / 100 : 0));
  }
  for (const node of status) {
    if (!node) continue;
    const width = measure(node);
    const required = width + (used ? gap : 0);
    const visible = used + required <= available * 0.49;
    setAttribute(node, 'data-xbar-header-hidden', String(!visible));
    if (visible) used += required;
  }
  bar.style.setProperty('--xbar-header-status-width', `${used}px`);
  const launcher = bar.querySelector('.buttons-container > .shell-menu');
  const showLauncher = available >= 480;
  if (launcher) setAttribute(launcher, 'data-xbar-header-hidden', String(!showLauncher));
  const launcherWidth = launcher && showLauncher ? launcher.getBoundingClientRect().width + gap : 0;
  const tags = [...bar.querySelectorAll('.buttons-container > .emoji-button')];
  const tagNatural = tags.reduce((width, node) => width + node.getBoundingClientRect().width, 0) + Math.max(0, tags.length - 1) * gap;
  const tagBudget = Math.min(available * 0.40, Math.max(32, available - used - launcherWidth - 80 - gap * 2));
  const leading = launcherWidth + Math.min(tagNatural, tagBudget);
  bar.style.setProperty('--xbar-header-leading-width', `${leading}px`);
  const title = bar.querySelector('.client-title');
  if (title) setAttribute(title, 'data-xbar-header-hidden', String(available - used - leading - gap * 2 < 80));
  const signature = `${bar.clientWidth}:${tags.map(node => `${node.dataset.tagIndex}:${node.getAttribute('aria-pressed')}`).join(',')}`;
  if (signature !== headerTagSignature) {
    headerTagSignature = signature;
    const selected = tags.find(node => node.getAttribute('aria-pressed') === 'true');
    const group = selected?.closest('.buttons-container');
    if (group) {
      const item = selected.getBoundingClientRect(), bounds = group.getBoundingClientRect();
      const delta = item.left < bounds.left ? item.left - bounds.left : item.right > bounds.right ? item.right - bounds.right : 0;
      group.scrollLeft += delta;
    }
  }
};
const queue = (content = true) => {
  contentDirty ||= content;
  if (!frame) frame = requestAnimationFrame(refresh);
};
const visibleDockRect = (element, rect = element.getBoundingClientRect()) => {
  if (!element.isConnected || !element.getClientRects().length) return null;
  let left = Math.max(0, rect.left);
  let top = Math.max(0, rect.top);
  let right = Math.min(innerWidth, rect.left + rect.width);
  let bottom = Math.min(innerHeight, rect.top + rect.height);
  for (let parent = element.parentElement; parent; parent = parent.parentElement) {
    const style = getComputedStyle(parent);
    const box = parent.getBoundingClientRect();
    if (/^(auto|scroll|hidden|clip)$/.test(style.overflowX)) {
      left = Math.max(left, box.left + parent.clientLeft);
      right = Math.min(right, box.left + parent.clientLeft + parent.clientWidth);
    }
    if (/^(auto|scroll|hidden|clip)$/.test(style.overflowY)) {
      top = Math.max(top, box.top + parent.clientTop);
      bottom = Math.min(bottom, box.top + parent.clientTop + parent.clientHeight);
    }
  }
  return right > left && bottom > top ? { left, top, width: right - left, height: bottom - top } : null;
};
const restingDockRect = (element, _dock) => {
  const rect = element.getBoundingClientRect();
  const style = getComputedStyle(element);
  const matrix = new DOMMatrixReadOnly(style.transform === 'none' ? undefined : style.transform);
  const [originX, originY] = style.transformOrigin.split(' ').map(Number.parseFloat);
  // Existing dock styles only translate and scale the item. Removing that
  // transform from the actual viewport rect also accounts for every scroll
  // ancestor, unlike offsetLeft/offsetTop added to the shelf's origin.
  const untransformed = matrix.b === 0 && matrix.c === 0 && matrix.a > 0 && matrix.d > 0
    ? {
      left: rect.left - originX * (1 - matrix.a) - matrix.e,
      top: rect.top - originY * (1 - matrix.d) - matrix.f,
      width: rect.width / matrix.a,
      height: rect.height / matrix.d,
    }
    : rect;
  return visibleDockRect(element, untransformed);
};
const keydown = event => {
  if (event.defaultPrevented || !(event.target instanceof Element) || event.target.closest(editableSelector)) return;
  const bar = event.target.closest('.button-row');
  if (!bar || event.altKey || event.ctrlKey || event.metaKey) return;
  const target = event.target.closest(controlSelector);
  if (!target) return;
  if (event.key === 'Escape') {
    const toggle = bar.querySelector('.layout-toggle.open');
    if (toggle && available(toggle)) {
      event.preventDefault();
      focusControl(toggle);
      toggle.click();
    }
    return;
  }
  if (target.matches('.volume-pill, .brightness-pill') && ['PageUp', 'PageDown', '+', '-'].includes(event.key)) {
    event.preventDefault();
    if (!available(target)) return;
    const increase = event.key === 'PageUp' || event.key === '+';
    // The framework keeps ownership of actions and IPC. Reuse its existing
    // wheel handler so keyboard and pointer steps cannot diverge by skin.
    adjustingMetric = true;
    try {
      target.dispatchEvent(new WheelEvent('wheel', { deltaY: increase ? -1 : 1, bubbles: true, cancelable: true }));
    } finally {
      adjustingMetric = false;
    }
    return;
  }
  if (event.key === 'Enter' || event.key === ' ') {
    if (!available(target) || event.repeat) event.preventDefault();
    else if (target.tagName !== 'BUTTON') {
      event.preventDefault();
      target.click();
    }
    return;
  }
  const rtl = getComputedStyle(bar).direction === 'rtl';
  const backward = vertical ? 'ArrowUp' : rtl ? 'ArrowRight' : 'ArrowLeft';
  const forward = vertical ? 'ArrowDown' : rtl ? 'ArrowLeft' : 'ArrowRight';
  if (![backward, forward, 'Home', 'End'].includes(event.key)) return;
  const controls = orderedControls(bar);
  const index = controls.indexOf(target);
  if (!controls.length) return;
  const next = event.key === 'Home' ? 0 : event.key === 'End' ? controls.length - 1 :
    index < 0 ? (event.key === backward ? controls.length - 1 : 0) :
    (index + (event.key === backward ? -1 : 1) + controls.length) % controls.length;
  event.preventDefault();
  focusControl(controls[next]);
};
const focusin = event => {
  const bar = event.target instanceof Element && event.target.closest('.button-row');
  const target = bar && event.target.closest(controlSelector);
  if (target) rememberFocus(bar, target);
  else toolbarHadFocus = false;
};
const focusout = event => {
  if (event.target !== focusedControl) return;
  if (event.relatedTarget) { toolbarHadFocus = false; return; }
  // Chromium can fire focusout just BEFORE disconnecting a removed node.
  // Check after that DOM operation so an explicit blur is distinguished from
  // reactive removal without losing the latter's focus-repair opportunity.
  const target = event.target;
  queueMicrotask(() => {
    if (focusedControl === target && target.isConnected && available(target) && document.activeElement !== target) toolbarHadFocus = false;
  });
};
const blur = () => { toolbarHadFocus = false; wheelGesture = null; metricWheelGesture = null; workspaceWheelGesture = null; };
const click = event => {
  const target = event.target instanceof Element && event.target.closest(controlSelector);
  if (target?.closest('.button-row') && (target.disabled || target.getAttribute('aria-disabled') === 'true' || target.classList.contains('shell-pill-offline'))) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }
};
const wheel = event => {
  if (adjustingMetric || !(event.target instanceof Element) || event.target.closest(editableSelector)) return;
  const bar = event.target.closest('.button-row');
  if (!bar) return;
  const workspace = event.target.closest('.emoji-button[data-tag-index]');
  if (workspace) {
    // The framework owns the original ID/monitor click binding. Cycling its
    // actual eligible DOM sequence avoids compressed indices and lets the
    // same gesture work in Dioxus and all six Tauri implementations.
    event.preventDefault();
    event.stopImmediatePropagation();
    wheelGesture = null;
    metricWheelGesture = null;
    const delta = Math.abs(event.deltaY) >= Math.abs(event.deltaX) ? event.deltaY : event.deltaX;
    if (!Number.isFinite(event.deltaX) || !Number.isFinite(event.deltaY)) { workspaceWheelGesture = null; return; }
    if (delta === 0 || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || !available(workspace)) return;
    const now = performance.now(), pixels = event.deltaMode === 0;
    if (!workspaceWheelGesture || now - workspaceWheelGesture.time >= scrollPolicy.idle_ms || workspaceWheelGesture.pixels !== pixels) {
      workspaceWheelGesture = { pixels, precise: false, fired: false, residual: 0, axis: null, time: now };
    }
    const gesture = workspaceWheelGesture;
    gesture.time = now;
    gesture.precise ||= pixels || !Number.isInteger(event.deltaX) || !Number.isInteger(event.deltaY);
    // As in native ControlScroll, a precision swipe owns one navigation
    // action, including momentum and snapshots that replace its tag nodes.
    if (gesture.precise && gesture.fired) return;
    const axis = Math.abs(event.deltaX) > Math.abs(event.deltaY) ? 'x' : 'y';
    if (gesture.axis !== axis) { gesture.residual = 0; gesture.axis = axis; }
    if (vertical && axis === 'x') { gesture.residual = 0; return; }
    const amount = delta / (pixels ? (scrollPolicy.navigation_pixels || 40) : 1);
    if (Math.sign(gesture.residual) !== Math.sign(amount)) gesture.residual = 0;
    gesture.residual += amount;
    if (Math.abs(gesture.residual) + 1e-9 < 1) return;
    const direction = Math.sign(gesture.residual);
    gesture.residual %= 1;
    gesture.fired = true;
    const tags = [...bar.querySelectorAll('.buttons-container > .emoji-button[data-tag-index]')].filter(available);
    if (!tags.length) return;
    let active = tags.findIndex(node => node.getAttribute('aria-pressed') === 'true' && node.dataset.tagIndex === bar.dataset.activeTag);
    if (active < 0) active = tags.findIndex(node => node.getAttribute('aria-pressed') === 'true');
    if (active < 0) active = 0;
    tags[(active + (direction < 0 ? -1 : 1) + tags.length) % tags.length].click();
    return;
  }
  const metric = event.target.closest('.volume-pill, .brightness-pill');
  const metricGesture = metric && !event.shiftKey && Math.abs(event.deltaY) > Math.abs(event.deltaX);
  if (event.ctrlKey || event.metaKey || event.altKey) {
    // Pinch-to-zoom and modified gestures must never change system settings.
    if (metric) event.stopImmediatePropagation();
    wheelGesture = null;
    metricWheelGesture = null;
    return;
  }
  const now = performance.now();
  let scroller = wheelGesture && now - wheelGesture.time < scrollPolicy.idle_ms &&
    wheelGesture.scroller.isConnected && bar.contains(wheelGesture.scroller) ? wheelGesture.scroller : null;
  if (!scroller && metricGesture && available(metric)) {
    event.preventDefault();
    const sign = Math.sign(event.deltaY);
    const unit = event.deltaMode;
    if (!metricWheelGesture || metricWheelGesture.target !== metric || now - metricWheelGesture.time >= scrollPolicy.idle_ms ||
        metricWheelGesture.sign !== sign || metricWheelGesture.unit !== unit) {
      metricWheelGesture = { target: metric, delta: 0, sign, unit, time: now };
    }
    metricWheelGesture.delta += event.deltaY;
    metricWheelGesture.time = now;
    const threshold = unit === 0 ? scrollPolicy.adjustment_pixels : 1;
    if (Math.abs(metricWheelGesture.delta) < threshold) event.stopImmediatePropagation();
    else {
      // Chromium commonly reports a mouse notch as 100px. At most one
      // adjustment per event preserves that notch while accumulating small
      // touchpad deltas instead of spawning one system command per pixel.
      metricWheelGesture.delta %= threshold;
    }
    return;
  }
  metricWheelGesture = null;
  if (metric) event.stopImmediatePropagation();
  const delta = vertical ? event.deltaY : Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
  if (!delta) return;
  if (!scroller) {
    for (let node = event.target; node && node !== bar; node = node.parentElement) {
      const style = getComputedStyle(node);
      const overflow = vertical ? style.overflowY : style.overflowX;
      const extent = vertical ? node.scrollHeight - node.clientHeight : node.scrollWidth - node.clientWidth;
      if (/^(auto|scroll)$/.test(overflow) && extent > 1) { scroller = node; break; }
    }
  }
  if (!scroller) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  // Once a gesture starts scrolling a group, keep that ownership through its
  // momentum tail, even if the moving content puts volume under the pointer.
  wheelGesture = { scroller, time: now };
  const unit = event.deltaMode === 1 ? parseFloat(getComputedStyle(scroller).lineHeight) || 16 :
    event.deltaMode === 2 ? (vertical ? scroller.clientHeight : scroller.clientWidth) : 1;
  scroller.scrollBy({ left: vertical ? 0 : delta * unit, top: vertical ? delta * unit : 0, behavior: 'instant' });
};
const scroll = event => {
  if (event.target instanceof Element && event.target.closest('.button-row')) queue(false);
};
const resize = () => queue(true);
// Observe only framework state attributes. Adapter-owned labels, tab stops,
// overflow markers and animation styles never feed back into this observer.
const observer = new MutationObserver(records => {
  // Some frameworks replace a focused subtree even for a snapshot update.
  // Restore focus and prepare its new legacy controls before the next input
  // event can target body. Ordinary telemetry and scrolling remain batched.
  if (toolbarHadFocus && document.hasFocus() && records.some(record => record.type === 'childList')) {
    const bar = document.querySelector('.button-row');
    if (bar) {
      refreshControls(bar);
      queue(false);
      return;
    }
  }
  queue(true);
});
observer.observe(document.body, {
  childList: true, subtree: true, characterData: true, attributes: true,
  attributeFilter: ['class', 'title', 'disabled', 'hidden', 'inert', 'aria-disabled', 'aria-pressed', 'aria-current'],
});
const resizeObserver = typeof ResizeObserver === 'function' ? new ResizeObserver(resize) : null;
document.addEventListener('keydown', keydown);
document.addEventListener('focusin', focusin);
document.addEventListener('focusout', focusout);
document.addEventListener('click', click, true);
document.addEventListener('wheel', wheel, { capture: true, passive: false });
document.addEventListener('scroll', scroll, true);
window.addEventListener('blur', blur);
window.addEventListener('resize', resize);
window.__xbarLayout = { restingDockRect, visibleDockRect,
  get entryControl() { return entryControl; }, get entryKey() { return entryKey; },
  get entryChosenByUser() { return entryChosenByUser; }, dispose() {
  observer.disconnect();
  resizeObserver?.disconnect();
  document.removeEventListener('keydown', keydown);
  document.removeEventListener('focusin', focusin);
  document.removeEventListener('focusout', focusout);
  document.removeEventListener('click', click, true);
  document.removeEventListener('wheel', wheel, true);
  document.removeEventListener('scroll', scroll, true);
  window.removeEventListener('blur', blur);
  window.removeEventListener('resize', resize);
  if (frame) cancelAnimationFrame(frame);
} };
contentDirty = true;
refresh();
