(() => {
  if (window.__xbarNativeScroll) return;
  const packets = [];
  const lifetime = 200;
  const sign = value => Math.sign(value || 0);
  const elapsed = (newer, older) => (newer - older + 4294967296) % 4294967296;
  let clock = null;
  const prune = now => {
    while (packets.length && now - packets[0].received > lifetime) packets.shift();
    if (clock && now - clock.received > 2147483648) clock = null;
  };
  window.__xbarNativeScroll = {
    record(packet) {
      if (!packet || !['line', 'pixel'].includes(packet.kind) ||
          ![packet.x, packet.y, packet.dx, packet.dy].every(Number.isFinite) ||
          !Number.isInteger(packet.nativeTime) || packet.nativeTime < 0 || packet.nativeTime > 4294967295) return;
      const now = performance.now();
      prune(now);
      packets.push({ ...packet, stop: !!packet.stop || (!packet.dx && !packet.dy), received: now });
      if (packets.length > 16) packets.splice(0, packets.length - 16);
    }
  };
  window.addEventListener('wheel', event => {
    // Native GDK time and DOM time have different origins. A matched pair
    // calibrates their offset; relative GDK timestamps then distinguish even
    // interleaved line/pixel packets with identical DOM deltas and positions.
    // Synthetic framework/keyboard events must neither consume nor calibrate.
    if (!event.isTrusted) return;
    const now = performance.now();
    prune(now);
    const candidates = [];
    for (let index = 0; index < packets.length; index++) {
      const packet = packets[index];
      if (!packet.stop && Math.abs(packet.x - event.clientX) <= 2 && Math.abs(packet.y - event.clientY) <= 2 &&
          sign(packet.dx) === sign(event.deltaX) && sign(packet.dy) === sign(event.deltaY)) candidates.push(index);
    }
    let index = -1;
    if (clock) {
      const predicted = packet => clock.dom + elapsed(packet.nativeTime, clock.native);
      let distance = 8;
      for (const candidate of candidates) {
        const packet = packets[candidate];
        const time = predicted(packet);
        const stopped = packets.some((boundary, boundaryIndex) => boundaryIndex > candidate && boundary.stop && predicted(boundary) < event.timeStamp - 0.5 &&
          elapsed(boundary.nativeTime, packet.nativeTime) < 2147483648);
        const delta = Math.abs(time - event.timeStamp);
        if (!stopped && delta < distance) { index = candidate; distance = delta; }
      }
    } else if (candidates.length && candidates.every(candidate => packets[candidate].kind === packets[candidates[0]].kind)) {
      // Do not guess the first source when multiple kinds are ambiguous.
      const candidate = candidates[0];
      if (!packets.slice(candidate + 1).some(packet => packet.stop)) index = candidate;
    }
    if (index < 0) { packets.length = 0; return; }
    const packet = packets[index];
    packets.splice(0, index + 1);
    clock = { native: packet.nativeTime, dom: event.timeStamp, received: now };
    if (packet.kind !== 'line' || !(event.target instanceof Element)) return;
    const workspace = event.target.closest('.emoji-button[data-tag-index]');
    if (!workspace?.closest('.button-row')) return;
    // GTK's original direction is the source of truth. The shared workspace
    // handler already owns/default-prevents this target; other scrolling is
    // untouched. Reentrancy is prevented by the isTrusted guard above.
    event.preventDefault();
    event.stopImmediatePropagation();
    workspace.dispatchEvent(new WheelEvent('wheel', {
      deltaMode: 1, deltaX: packet.dx, deltaY: packet.dy,
      clientX: event.clientX, clientY: event.clientY,
      screenX: event.screenX, screenY: event.screenY,
      ctrlKey: event.ctrlKey, shiftKey: event.shiftKey,
      altKey: event.altKey, metaKey: event.metaKey,
      bubbles: true, cancelable: true
    }));
  }, { capture: true, passive: false });
})();
