(() => {
  let errors = 0;
  const report = (stage, message) => {
    const invoke = window.__TAURI_INTERNALS__?.invoke;
    if (typeof invoke === "function") {
      return invoke("frontend_diagnostic", {stage, message: String(message).slice(0, 2048)}).then(() => true, reason => {
        if (stage === "rendered") report("render-report-rejected", reason?.message || reason);
        return false;
      });
    }
    return Promise.resolve(false);
  };
  const error = value => {
    if (errors++ < 16) report("error", value?.stack || value?.message || value);
  };
  window.addEventListener("error", event => {
    error(event.error || `${event.message || "resource load failure"} ${event.filename || event.target?.src || event.target?.href || ""}:${event.lineno || 0}`);
  }, true);
  window.addEventListener("unhandledrejection", event => error(event.reason));
  const consoleError = console.error.bind(console);
  console.error = (...args) => {
    consoleError(...args);
    error(args.map(value => String(value)).join(" "));
  };
  let visibilityChanges = 0;
  const visibility = stage => {
    // Diagnostics must not prevent the original double-rAF acknowledgement.
    try {
      report(stage, JSON.stringify({visibilityState: document.visibilityState,
        hidden: document.hidden, width: window.innerWidth, height: window.innerHeight}));
    } catch (_) {}
  };
  document.addEventListener("visibilitychange", () => {
    if (visibilityChanges++ < 16) visibility("visibility");
  });
  let rendered = false;
  let phase = "waiting-dom";
  // Bounded observations only: these never advance the render gate. Use the
  // existing BOOT property so the native failure artifact captures the last
  // phase without introducing an unsupported diagnostic command stage.
  const diagnosticTimers = [5000, 20000, 45000].map(delay => setTimeout(() => {
    const row = document.querySelector(".button-row");
    report("boot", JSON.stringify({
      phase,
      readyState: document.readyState,
      visibilityState: document.visibilityState,
      clock: !!document.querySelector(".time-pill"),
      emoji: !!document.querySelector(".emoji-button"),
      row: !!row,
      loading: row?.textContent?.trim() === "Loading...",
    }));
  }, delay));
  const observer = new MutationObserver(() => {
    if (rendered || !document.querySelector(".time-pill") || !document.querySelector(".emoji-button") || document.querySelector(".button-row")?.textContent?.trim() === "Loading...") return;
    rendered = true;
    observer.disconnect();
    // Independent properties retain each milestone even if IPC completes out of order.
    // These probes are evidence only: rendered still requires both animation frames.
    phase = "waiting-frame-1";
    visibility("dom-ready");
    requestAnimationFrame(() => {
      phase = "waiting-frame-2";
      report("raf-first", "first animation frame reached");
      requestAnimationFrame(() => {
        report("raf-second", "second animation frame reached");
        phase = "render-ipc-pending";
        report("rendered", "state projected into workspace controls and clock").then(ok => {
          phase = ok ? "render-ipc-acknowledged" : "render-ipc-failed";
          if (ok) diagnosticTimers.forEach(clearTimeout);
        });
      });
    });
  });
  observer.observe(document, {childList: true, subtree: true});
  report("boot", "WebView initialization script loaded");
})();
