(() => {
  let errors = 0;
  const report = (stage, message) => {
    const invoke = window.__TAURI_INTERNALS__?.invoke;
    if (typeof invoke === "function") {
      invoke("frontend_diagnostic", {stage, message: String(message).slice(0, 2048)}).catch(reason => {
        if (stage === "rendered") report("render-report-rejected", reason?.message || reason);
      });
    }
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
  const observer = new MutationObserver(() => {
    if (rendered || !document.querySelector(".time-pill") || !document.querySelector(".emoji-button") || document.querySelector(".button-row")?.textContent?.trim() === "Loading...") return;
    rendered = true;
    observer.disconnect();
    // Independent properties retain each milestone even if IPC completes out of order.
    // These probes are evidence only: rendered still requires both animation frames.
    visibility("dom-ready");
    requestAnimationFrame(() => {
      report("raf-first", "first animation frame reached");
      requestAnimationFrame(() => {
        report("raf-second", "second animation frame reached");
        report("rendered", "state projected into workspace controls and clock");
      });
    });
  });
  observer.observe(document, {childList: true, subtree: true});
  report("boot", "WebView initialization script loaded");
})();
