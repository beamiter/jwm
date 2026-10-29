(() => {
  let errors = 0;
  const report = (stage, message) => {
    const invoke = window.__TAURI_INTERNALS__?.invoke;
    if (typeof invoke === "function") {
      invoke("frontend_diagnostic", {stage, message: String(message).slice(0, 2048)}).catch(() => {});
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
  let rendered = false;
  const observer = new MutationObserver(() => {
    if (rendered || !document.querySelector(".time-pill") || !document.querySelector(".emoji-button") || document.querySelector(".button-row")?.textContent?.trim() === "Loading...") return;
    rendered = true;
    observer.disconnect();
    requestAnimationFrame(() => requestAnimationFrame(() => report("rendered", "state projected into workspace controls and clock")));
  });
  observer.observe(document, {childList: true, subtree: true});
  report("boot", "WebView initialization script loaded");
})();
