// `edge` and `thickness` are supplied by the Rust host. This installer can be
// replayed on scale changes/readiness without accumulating global listeners.
const previous = window.__xbarLayout;
if (previous) previous.dispose();
const root = document.documentElement;
root.dataset.barEdge = edge;
root.style.setProperty('--xbar-thickness', `${thickness}px`);
const vertical = edge === 'left' || edge === 'right';
const legacyButtons = '.emoji-button, .layout-toggle, .layout-option, .volume-pill, .brightness-pill, ' +
  '.shell-pill, .shell-hub-pill, .shell-route, .screenshot-pill, .time-pill, [data-window-id]';
let frame = 0;
const refresh = () => {
  frame = 0;
  const bar = document.querySelector('.button-row');
  if (!bar) return;
  bar.setAttribute('role', 'toolbar');
  bar.setAttribute('aria-label', 'JWM status bar');
  bar.setAttribute('aria-orientation', vertical ? 'vertical' : 'horizontal');
  for (const target of bar.querySelectorAll(legacyButtons)) {
    if (target.tagName !== 'BUTTON' && !target.hasAttribute('role')) {
      target.setAttribute('role', 'button');
      target.tabIndex = 0;
    }
    if (target.title && !target.dataset.windowId) {
      target.setAttribute('aria-label', target.title);
    }
    if (target.classList.contains('shell-pill-offline')) target.setAttribute('aria-disabled', 'true');
    else if (target.matches('.shell-pill, .shell-hub-pill')) target.removeAttribute('aria-disabled');
  }
  for (const clock of bar.querySelectorAll('.time-pill')) {
    const full = clock.textContent.trim();
    const match = full.match(/(\d{1,2}):(\d{2})(?::(\d{2}))?/);
    if (match) {
      clock.dataset.sidebarLabel = match.slice(1).filter(Boolean).join('\n');
      clock.setAttribute('aria-label', `Time ${match[0]}`);
      clock.title = match[0];
    }
  }
};
const queue = () => {
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
// Follow the displayed flex order, including the priority status group. Using
// screen coordinates would change navigation whenever a group is scrolled.
const orderedControls = bar => {
  const controls = [];
  const visit = node => {
    if (node.matches('button, [role="button"]')) {
      if (!node.disabled && node.getAttribute('aria-disabled') !== 'true' && node.getClientRects().length) {
        controls.push(node);
      }
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
const keydown = event => {
  if (event.defaultPrevented) return;
  const bar = event.target.closest('.button-row');
  if (!bar || event.altKey || event.ctrlKey || event.metaKey) return;
  const target = event.target.closest('button, [role="button"]');
  if (!target) return;
  if ((event.key === 'Enter' || event.key === ' ') && target.tagName !== 'BUTTON') {
    event.preventDefault();
    if (target.getAttribute('aria-disabled') !== 'true') target.click();
    return;
  }
  const backward = vertical ? 'ArrowUp' : 'ArrowLeft';
  const forward = vertical ? 'ArrowDown' : 'ArrowRight';
  if (![backward, forward, 'Home', 'End'].includes(event.key)) return;
  const controls = orderedControls(bar);
  const index = controls.indexOf(target);
  if (!controls.length) return;
  const next = event.key === 'Home' ? 0 : event.key === 'End' ? controls.length - 1 :
    index < 0 ? (event.key === backward ? controls.length - 1 : 0) :
    (index + (event.key === backward ? -1 : 1) + controls.length) % controls.length;
  event.preventDefault();
  controls[next].focus({ preventScroll: true });
  controls[next].scrollIntoView({ block: 'nearest', inline: 'nearest' });
};
// Only content changes are observed: changing ARIA/layout attributes above
// must never cause an observer feedback loop or extra work on idle bars.
const observer = new MutationObserver(queue);
observer.observe(document.body, { childList: true, subtree: true, characterData: true });
document.addEventListener('keydown', keydown);
window.addEventListener('resize', queue);
window.__xbarLayout = { restingDockRect, visibleDockRect, dispose() {
  observer.disconnect();
  document.removeEventListener('keydown', keydown);
  window.removeEventListener('resize', queue);
  if (frame) cancelAnimationFrame(frame);
} };
refresh();
