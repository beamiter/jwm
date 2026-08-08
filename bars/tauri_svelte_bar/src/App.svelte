<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";

  interface TagState {
    selected: boolean;
    urgent: boolean;
    filled: boolean;
    occupied: boolean;
  }

  interface AudioDeviceInfo {
    name: string;
    volume: number;
    is_muted: boolean;
  }

  interface SystemDetails {
    cpu_average: number;
    memory_total: number;
    memory_used: number;
    memory_usage_percent: number;
  }

  interface BatteryState {
    percent: number | null;
    charging: boolean;
    present: boolean;
  }

  interface MinimizedWindow {
    token: number;
    monitor: number;
    title: string;
    app_id: string;
    flags: number;
  }

  interface DockGeometry {
    x: number;
    y: number;
    width: number;
    height: number;
  }

  interface BarSnapshot {
    wm_available: boolean;
    wm_sequence: number | null;
    wm_session_id: number;
    geometry: DockGeometry | null;
    tags: TagState[];
    monitor: number;
    layout_symbol: string;
    client_name: string;
    time: string;
    show_seconds: boolean;
    layout_selector_open: boolean;
    audio_device: AudioDeviceInfo | null;
    system_details: SystemDetails;
    brightness: { percent: number | null };
    battery: BatteryState;
    minimized_windows: MinimizedWindow[];
    minimized_overflow: boolean;
  }

  interface FrontendEnvelope {
    revision: number;
    changes: number;
    snapshot: BarSnapshot;
    partition_changes?: number;
  }

  type ShellRoute =
    | "hub"
    | "applications"
    | "notifications"
    | "clipboard"
    | "calendar"
    | "wallpaper";

  // Pages of JWM's own shell surface, in the order the hub itself lists them.
  // Every entry is one request to the window manager: the bar renders no shell
  // content and keeps no shell state.
  const SHELL_ROUTES: { route: ShellRoute; icon: string; label: string }[] = [
    { route: "hub", icon: "\u{F0F2A}", label: "Shell Hub" },
    { route: "applications", icon: "\u{F0D22}", label: "Applications" },
    { route: "notifications", icon: "\u{F009A}", label: "Notifications" },
    { route: "clipboard", icon: "\u{F0192}", label: "Clipboard" },
    { route: "calendar", icon: "\u{F00ED}", label: "Calendar" },
    { route: "wallpaper", icon: "\u{F02E9}", label: "Wallpaper" },
  ];

  type ActionRequest =
    | { action: "view_tag_on"; tag_index: number; monitor_id: number }
    | { action: "toggle_layout_selector" }
    | { action: "set_layout_on"; layout_id: number; monitor_id: number }
    | { action: "toggle_seconds" }
    | { action: "toggle_mute" }
    | { action: "adjust_volume"; delta: number }
    | { action: "adjust_brightness"; delta: number }
    | { action: "screenshot" }
    | {
        action: "restore_window";
        wm_session_id: number;
        minimized_generation: number;
        window_id: number;
        geometry?: DockGeometry;
      }
    | {
        action: "preview_window";
        wm_session_id: number;
        minimized_generation: number;
        window_id: number;
        visible: boolean;
        renewal: boolean;
        geometry?: DockGeometry;
      }
    | {
        action: "set_dock_geometry";
        wm_session_id: number;
        minimized_generation: number;
        window_id?: number | null;
        geometry?: DockGeometry;
      }
    | { action: "open_shell_hub"; route: ShellRoute };

  const dispatchAction = (request: ActionRequest): Promise<void> =>
    invoke("dispatch_action", { request });

  let snapshotBarOrigin: Pick<DockGeometry, "x" | "y"> | null = null;
  let currentWmSessionId = 0;
  let currentMinimizedGeneration = 0;
  let currentWmAvailable = false;

  const TAG_ICONS = [
    "\u{F0A1E}",
    "\u{F0239}",
    "\u{F0A1B}",
    "\u{F0B79}",
    "\u{F024B}",
    "\u{F0388}",
    "\u{F0567}",
    "\u{F01F0}",
    "\u{F0297}",
  ];

  const ICON_CPU = "\u{F4BC}";
  const ICON_MEM = "\u{F035B}";
  const ICON_BAT_FULL = "\u{F0079}";
  const ICON_BAT_CHG = "\u{F0084}";
  const ICON_VOL_HIGH = "\u{F057E}";
  const ICON_VOL_MID = "\u{F0580}";
  const ICON_VOL_LOW = "\u{F057F}";
  const ICON_VOL_MUTE = "\u{F075F}";
  const ICON_BRIGHT = "\u{F00DE}";
  const ICON_SHOT = "\u{F0104}";
  const ICON_TIME = "\u{F0954}";
  const ICON_MON = "\u{F0379}";

  let snapshot = $state<BarSnapshot | null>(null);
  let scaleFactor = $state<number | null>(null);
  let pressed = $state<number | null>(null);
  let isTaking = $state(false);

  let cancelled = false;
  let revision: number | null = null;
  let unlisten: UnlistenFn | undefined;
  let dockGeometrySignature = "";
  let dockResizeObserver: ResizeObserver | undefined;
  let dockRetryTimer: number | undefined;
  let dockPublishGeneration = 0;
  let dockPublishInFlight = false;
  let dockRepublishRequested = false;
  const PREVIEW_ENTER_RETRY_MS = 100;
  const PREVIEW_RENEWAL_MS = 2_000;
  // Fresh ENTER remains fresh until the invoke is acknowledged.
  const PREVIEW_ENTER_REQUEST = { visible: true, renewal: false } as const;
  const PREVIEW_RENEWAL_REQUEST = { visible: true, renewal: true } as const;

  interface PreviewDelivery {
    windowId: number;
    wmSessionId: number;
    minimizedGeneration: number;
    element: HTMLElement;
    retryTimer?: number;
    renewalTimer?: number;
  }

  const previewDeliveries = new Map<string, PreviewDelivery>();

  function previewBindingKey(
    windowId: number,
    wmSessionId: number,
    minimizedGeneration: number,
  ): string {
    return `${wmSessionId}:${minimizedGeneration}:${windowId}`;
  }

  function getButtonClass(tag: TagState): string {
    if (tag.filled) return "emoji-button state-filtered";
    if (tag.selected) return "emoji-button state-selected";
    if (tag.urgent) return "emoji-button state-urgent";
    if (tag.occupied) return "emoji-button state-occupied";
    return "emoji-button state-default";
  }

  function formatBytes(bytes: number): string {
    if (bytes === 0) return "0B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const index = Math.min(
      units.length - 1,
      Math.floor(Math.log(bytes) / Math.log(1024)),
    );
    const size = Number(
      (bytes / Math.pow(1024, index)).toFixed(index === 0 ? 0 : 1),
    );
    return `${size}${units[index]}`;
  }

  function severity(percent: number): string {
    return percent <= 30
      ? "usage-good"
      : percent <= 60
        ? "usage-warn"
        : percent <= 80
          ? "usage-caution"
          : "usage-danger";
  }

  function monitorIcon(monitor: number): string {
    if (monitor === 0) return "\u{F02DA}";
    if (monitor === 1) return "\u{F02DB}";
    return `M${monitor}`;
  }

  function volumeIcon(device: AudioDeviceInfo | null): string {
    if (!device || device.is_muted || device.volume <= 0) {
      return ICON_VOL_MUTE;
    }
    if (device.volume < 34) return ICON_VOL_LOW;
    if (device.volume < 67) return ICON_VOL_MID;
    return ICON_VOL_HIGH;
  }

  function minimizedInitial(item: MinimizedWindow): string {
    const label = item.app_id.trim() || item.title.trim();
    return Array.from(label)[0]?.toLocaleUpperCase() ?? "•";
  }

  type WindowMetrics = { x: number; y: number; scale: number };

  async function windowMetrics(): Promise<WindowMetrics | null> {
    try {
      const appWindow = getCurrentWindow();
      const scale = await appWindow.scaleFactor();
      if (snapshotBarOrigin) {
        return { x: snapshotBarOrigin.x, y: snapshotBarOrigin.y, scale };
      }
      const origin = await appWindow.innerPosition();
      return { x: origin.x, y: origin.y, scale };
    } catch (error) {
      console.error("Failed to resolve dock geometry:", error);
      return null;
    }
  }

  function projectDockGeometry(
    rect: Pick<DOMRect, "left" | "top" | "width" | "height">,
    metrics: WindowMetrics,
  ): DockGeometry {
    return {
      x: metrics.x + Math.round(rect.left * metrics.scale),
      y: metrics.y + Math.round(rect.top * metrics.scale),
      width: Math.max(0, Math.round(rect.width * metrics.scale)),
      height: Math.max(0, Math.round(rect.height * metrics.scale)),
    };
  }

  function physicalGeometry(element: HTMLElement, metrics: WindowMetrics): DockGeometry {
    return projectDockGeometry(element.getBoundingClientRect(), metrics);
  }

  function restingItemGeometry(
    item: HTMLElement,
    dock: HTMLElement,
    metrics: WindowMetrics,
  ): DockGeometry {
    const dockRect = dock.getBoundingClientRect();
    return projectDockGeometry(
      {
        left: dockRect.left + item.offsetLeft,
        top: dockRect.top + item.offsetTop,
        width: item.offsetWidth,
        height: item.offsetHeight,
      },
      metrics,
    );
  }

  async function geometryForElement(
    element: HTMLElement,
  ): Promise<DockGeometry | undefined> {
    const metrics = await windowMetrics();
    return metrics ? physicalGeometry(element, metrics) : undefined;
  }

  async function restoreMinimized(
    windowId: number,
    wmSessionId: number,
    minimizedGeneration: number,
    element: HTMLElement,
  ) {
    const geometry = await geometryForElement(element);
    await dispatchAction({
      action: "restore_window",
      wm_session_id: wmSessionId,
      minimized_generation: minimizedGeneration,
      window_id: windowId,
      geometry,
    });
  }

  async function previewMinimized(
    windowId: number,
    wmSessionId: number,
    minimizedGeneration: number,
    visible: boolean,
    renewal: boolean,
    element: HTMLElement,
  ) {
    const geometry = await geometryForElement(element);
    if (visible && !element.matches(":hover")) return;
    await dispatchAction({
      action: "preview_window",
      wm_session_id: wmSessionId,
      minimized_generation: minimizedGeneration,
      window_id: windowId,
      visible,
      renewal,
      geometry,
    });
  }

  function stopPreviewDelivery(bindingKey: string, expected?: PreviewDelivery) {
    const delivery = previewDeliveries.get(bindingKey);
    if (!delivery || (expected !== undefined && delivery !== expected)) return;
    previewDeliveries.delete(bindingKey);
    if (delivery.retryTimer !== undefined) window.clearTimeout(delivery.retryTimer);
    if (delivery.renewalTimer !== undefined) window.clearInterval(delivery.renewalTimer);
  }

  function stopAllPreviewDeliveries() {
    for (const [bindingKey, delivery] of Array.from(previewDeliveries)) {
      sendAutomaticPreviewLeave(bindingKey, delivery);
    }
  }

  function stopInvalidPreviewDeliveries(current: BarSnapshot) {
    const currentTokens = new Set(current.minimized_windows.map((item) => item.token));
    const minimizedGeneration = current.wm_sequence ?? 0;
    for (const [bindingKey, delivery] of Array.from(previewDeliveries)) {
      if (
        delivery.wmSessionId !== current.wm_session_id ||
        delivery.minimizedGeneration !== minimizedGeneration ||
        !currentTokens.has(delivery.windowId)
      ) {
        sendAutomaticPreviewLeave(bindingKey, delivery);
      }
    }
  }

  function previewDeliveryIsActive(bindingKey: string, delivery: PreviewDelivery) {
    return (
      previewDeliveries.get(bindingKey) === delivery &&
      delivery.element.isConnected &&
      delivery.element.matches(":hover")
    );
  }

  async function sendPreviewLeaveUnlessReentered(
    bindingKey: string,
    delivery: PreviewDelivery,
  ) {
    const geometry = await geometryForElement(delivery.element);
    const current = previewDeliveries.get(bindingKey);
    if (current !== undefined && current !== delivery) return;
    await dispatchAction({
      action: "preview_window",
      wm_session_id: delivery.wmSessionId,
      minimized_generation: delivery.minimizedGeneration,
      window_id: delivery.windowId,
      visible: false,
      renewal: false,
      geometry,
    });
  }

  function sendAutomaticPreviewLeave(bindingKey: string, delivery: PreviewDelivery) {
    if (previewDeliveries.get(bindingKey) !== delivery) return;
    stopPreviewDelivery(bindingKey, delivery);
    sendPreviewLeaveUnlessReentered(bindingKey, delivery).catch(console.error);
  }

  function compensateDeliveredPreviewEnter(bindingKey: string, delivery: PreviewDelivery) {
    const current = previewDeliveries.get(bindingKey);
    if (current !== undefined && current !== delivery) return;
    if (current === delivery) stopPreviewDelivery(bindingKey, delivery);
    // Compensate a delivered ENTER that outlived its binding.
    sendPreviewLeaveUnlessReentered(bindingKey, delivery).catch(console.error);
  }

  function schedulePreviewEnterRetry(
    bindingKey: string,
    delivery: PreviewDelivery,
    error: unknown,
  ) {
    if (previewDeliveries.get(bindingKey) !== delivery) return;
    console.error("Failed to enter minimized preview; retrying:", error);
    if (!previewDeliveryIsActive(bindingKey, delivery)) {
      sendAutomaticPreviewLeave(bindingKey, delivery);
      return;
    }
    if (delivery.renewalTimer !== undefined) {
      window.clearInterval(delivery.renewalTimer);
      delivery.renewalTimer = undefined;
    }
    if (delivery.retryTimer !== undefined) return;
    delivery.retryTimer = window.setTimeout(() => {
      delivery.retryTimer = undefined;
      deliverPreviewEnter(bindingKey, delivery);
    }, PREVIEW_ENTER_RETRY_MS);
  }

  function startPreviewRenewal(bindingKey: string, delivery: PreviewDelivery) {
    if (!previewDeliveryIsActive(bindingKey, delivery) || delivery.renewalTimer !== undefined) return;
    delivery.renewalTimer = window.setInterval(() => {
      if (!previewDeliveryIsActive(bindingKey, delivery)) {
        sendAutomaticPreviewLeave(bindingKey, delivery);
        return;
      }
      previewMinimized(
        delivery.windowId,
        delivery.wmSessionId,
        delivery.minimizedGeneration,
        PREVIEW_RENEWAL_REQUEST.visible,
        PREVIEW_RENEWAL_REQUEST.renewal,
        delivery.element,
      ).catch((error) => schedulePreviewEnterRetry(bindingKey, delivery, error));
    }, PREVIEW_RENEWAL_MS);
  }

  function deliverPreviewEnter(bindingKey: string, delivery: PreviewDelivery) {
    if (!previewDeliveryIsActive(bindingKey, delivery)) {
      sendAutomaticPreviewLeave(bindingKey, delivery);
      return;
    }
    previewMinimized(
      delivery.windowId,
      delivery.wmSessionId,
      delivery.minimizedGeneration,
      PREVIEW_ENTER_REQUEST.visible,
      PREVIEW_ENTER_REQUEST.renewal,
      delivery.element,
    )
      .then(() => {
        if (previewDeliveryIsActive(bindingKey, delivery)) {
          startPreviewRenewal(bindingKey, delivery);
        } else {
          compensateDeliveredPreviewEnter(bindingKey, delivery);
        }
      })
      .catch((error) => schedulePreviewEnterRetry(bindingKey, delivery, error));
  }

  function beginPreview(
    windowId: number,
    wmSessionId: number,
    minimizedGeneration: number,
    element: HTMLElement,
  ) {
    const bindingKey = previewBindingKey(windowId, wmSessionId, minimizedGeneration);
    stopPreviewDelivery(bindingKey);
    const delivery: PreviewDelivery = {
      windowId,
      wmSessionId,
      minimizedGeneration,
      element,
    };
    previewDeliveries.set(bindingKey, delivery);
    deliverPreviewEnter(bindingKey, delivery);
  }

  function endPreview(
    windowId: number,
    wmSessionId: number,
    minimizedGeneration: number,
    element: HTMLElement,
  ) {
    const bindingKey = previewBindingKey(windowId, wmSessionId, minimizedGeneration);
    const delivery = previewDeliveries.get(bindingKey);
    if (delivery) {
      sendAutomaticPreviewLeave(bindingKey, delivery);
    } else {
      sendPreviewLeaveUnlessReentered(bindingKey, {
        windowId,
        wmSessionId,
        minimizedGeneration,
        element,
      }).catch(console.error);
    }
  }

  async function publishDockGeometry(
    dock: HTMLElement,
    wmSessionId: number,
    minimizedGeneration: number,
  ) {
    const metrics = await windowMetrics();
    if (!dock.isConnected) return;
    if (!metrics) throw new Error("Dock window metrics are temporarily unavailable");
    await dispatchAction({
      action: "set_dock_geometry",
      wm_session_id: wmSessionId,
      minimized_generation: minimizedGeneration,
      window_id: null,
      geometry: physicalGeometry(dock, metrics),
    });
    for (const item of dock.querySelectorAll<HTMLElement>("[data-window-id]")) {
      const windowId = Number(item.dataset.windowId);
      if (!Number.isFinite(windowId)) continue;
      await dispatchAction({
        action: "set_dock_geometry",
        wm_session_id: wmSessionId,
        minimized_generation: minimizedGeneration,
        window_id: windowId,
        geometry: restingItemGeometry(item, dock, metrics),
      });
    }
  }

  function cancelDockGeometryRetry() {
    dockPublishGeneration += 1;
    if (dockRetryTimer !== undefined) window.clearTimeout(dockRetryTimer);
    dockRetryTimer = undefined;
    dockPublishInFlight = false;
    dockRepublishRequested = false;
  }

  function requestDockGeometryPublish(
    dock: HTMLElement,
    wmSessionId: number,
    minimizedGeneration: number,
  ) {
    if (
      !dock.isConnected ||
      !currentWmAvailable ||
      wmSessionId === 0 ||
      wmSessionId !== currentWmSessionId ||
      minimizedGeneration !== currentMinimizedGeneration
    ) return;
    const generation = dockPublishGeneration;
    if (dockRetryTimer !== undefined) {
      window.clearTimeout(dockRetryTimer);
      dockRetryTimer = undefined;
    }
    if (dockPublishInFlight) {
      dockRepublishRequested = true;
      return;
    }
    dockPublishInFlight = true;
    publishDockGeometry(dock, wmSessionId, minimizedGeneration)
      .then(() => {
        if (generation !== dockPublishGeneration) return;
        dockPublishInFlight = false;
        if (dockRepublishRequested) {
          dockRepublishRequested = false;
          requestDockGeometryPublish(dock, wmSessionId, minimizedGeneration);
        }
      })
      .catch((error) => {
        if (generation !== dockPublishGeneration) return;
        dockPublishInFlight = false;
        dockRepublishRequested = false;
        console.error("Failed to publish minimized Dock geometry; retrying:", error);
        if (
          dockRetryTimer === undefined &&
          dock.isConnected &&
          currentWmAvailable &&
          wmSessionId !== 0 &&
          wmSessionId === currentWmSessionId &&
          minimizedGeneration === currentMinimizedGeneration
        ) {
          dockRetryTimer = window.setTimeout(() => {
            dockRetryTimer = undefined;
            requestDockGeometryPublish(dock, wmSessionId, minimizedGeneration);
          }, 100);
        }
      });
  }

  function scheduleDockGeometry(current: BarSnapshot) {
    if (!current.wm_available || current.wm_session_id === 0) {
      cancelDockGeometryRetry();
      dockResizeObserver?.disconnect();
      dockGeometrySignature = "";
      return;
    }
    const minimizedGeneration = current.wm_sequence ?? 0;
    const signature = `${current.wm_session_id}|${minimizedGeneration}|${current.geometry?.x},${current.geometry?.y},${current.geometry?.width},${current.geometry?.height}|${current.minimized_windows
      .map((item) => item.token)
      .join(",")}|${current.minimized_overflow}`;
    if (signature === dockGeometrySignature) return;
    dockGeometrySignature = signature;
    window.requestAnimationFrame(() => {
      const dock = document.querySelector<HTMLElement>(".minimized-dock");
      if (!dock) return;
      requestDockGeometryPublish(dock, current.wm_session_id, minimizedGeneration);
      dockResizeObserver?.disconnect();
      if (typeof ResizeObserver !== "undefined") {
        dockResizeObserver = new ResizeObserver(() =>
          requestDockGeometryPublish(dock, current.wm_session_id, minimizedGeneration),
        );
        dockResizeObserver.observe(dock);
      }
    });
  }

  function batteryPercent(current: BarSnapshot): number | null {
    return current.battery.present ? current.battery.percent : null;
  }

  function batteryClass(current: BarSnapshot): string {
    const percent = batteryPercent(current);
    if (percent === null) return "usage-warn";
    if (percent > 50) return "usage-good";
    if (percent > 20) return "usage-warn";
    return "usage-danger";
  }

  function batteryTitle(current: BarSnapshot): string {
    const percent = batteryPercent(current);
    if (percent === null) return "未检测到电池";
    return current.battery.charging
      ? `电池充电中: ${percent.toFixed(1)}%`
      : `电池电量: ${percent.toFixed(1)}%`;
  }

  function batteryLabel(current: BarSnapshot): string {
    const percent = batteryPercent(current);
    return percent === null ? "--" : `${percent.toFixed(0)}%`;
  }

  function handleRelease(index: number, monitor: number) {
    pressed = null;
    dispatchAction({
      action: "view_tag_on",
      tag_index: index,
      monitor_id: monitor,
    }).catch(console.error);
  }

  function toggleLayoutSelector() {
    dispatchAction({ action: "toggle_layout_selector" }).catch(console.error);
  }

  function selectLayout(layoutId: number, monitor: number) {
    dispatchAction({
      action: "set_layout_on",
      layout_id: layoutId,
      monitor_id: monitor,
    }).catch(console.error);
  }

  function adjustBrightness(delta: number) {
    dispatchAction({ action: "adjust_brightness", delta }).catch(console.error);
  }

  function toggleMute() {
    dispatchAction({ action: "toggle_mute" }).catch(console.error);
  }

  function toggleSeconds() {
    dispatchAction({ action: "toggle_seconds" }).catch(console.error);
  }

  function openShell(route: ShellRoute) {
    // The shell lives in the window manager, so a click with no projection has
    // nowhere to go; the pill is grayed out rather than silently inert.
    if (!snapshot?.wm_available) return;
    dispatchAction({ action: "open_shell_hub", route }).catch(console.error);
  }

  async function takeScreenshot() {
    if (isTaking) return;
    isTaking = true;
    try {
      await dispatchAction({ action: "screenshot" });
    } catch (error) {
      console.error(error);
    } finally {
      window.setTimeout(() => (isTaking = false), 500);
    }
  }

  onMount(() => {
    const handleResize = () => {
      const dock = document.querySelector<HTMLElement>(".minimized-dock");
      if (dock && snapshot) {
        requestDockGeometryPublish(
          dock,
          snapshot.wm_session_id,
          snapshot.wm_sequence ?? 0,
        );
      }
    };
    window.addEventListener("resize", handleResize);
    const initialize = async () => {
      const stopListening = await listen<FrontendEnvelope>(
        "xbar-state",
        (event) => {
          if (cancelled) return;
          if (revision !== null && event.payload.revision < revision) return;
          revision = event.payload.revision;
          snapshotBarOrigin = event.payload.snapshot.geometry;
          if (
            currentWmSessionId !== event.payload.snapshot.wm_session_id ||
            currentMinimizedGeneration !== (event.payload.snapshot.wm_sequence ?? 0) ||
            currentWmAvailable !== event.payload.snapshot.wm_available
          ) {
            cancelDockGeometryRetry();
            stopAllPreviewDeliveries();
          }
          stopInvalidPreviewDeliveries(event.payload.snapshot);
          currentWmSessionId = event.payload.snapshot.wm_session_id;
          currentMinimizedGeneration = event.payload.snapshot.wm_sequence ?? 0;
          currentWmAvailable = event.payload.snapshot.wm_available;
          snapshot = event.payload.snapshot;
          scheduleDockGeometry(event.payload.snapshot);
        },
      );
      if (cancelled) {
        stopListening();
        return;
      }
      unlisten = stopListening;

      try {
        scaleFactor = await getCurrentWindow().scaleFactor();
      } catch (error) {
        console.error("Failed to query the Tauri window scale factor:", error);
      }
      await invoke<void>("frontend_ready");
    };

    initialize().catch((error) => {
      console.error("Failed to initialize xbar Tauri bridge:", error);
    });

    return () => window.removeEventListener("resize", handleResize);
  });

  onDestroy(() => {
    cancelled = true;
    unlisten?.();
    dockResizeObserver?.disconnect();
    cancelDockGeometryRetry();
    stopAllPreviewDeliveries();
  });
</script>

{#if !snapshot}
  <div class="button-row">Loading...</div>
{:else}
  {@const monitorId = snapshot.monitor}
  {@const wmSessionId = snapshot.wm_session_id}
  {@const minimizedGeneration = snapshot.wm_sequence ?? 0}
  <div class="button-row">
    <div class="buttons-container">
      {#each TAG_ICONS as icon, index}
        {@const tag = snapshot.tags[index] ?? {
          selected: false,
          urgent: false,
          filled: false,
          occupied: false,
        }}
        <button
          class={`${getButtonClass(tag)}${pressed === index ? " pressed" : ""}`}
          onmousedown={() => (pressed = index)}
          onmouseup={() => handleRelease(index, monitorId)}
          onmouseleave={() => (pressed = null)}
          title={`Tag ${index + 1}`}
        >
          <span class="nf-icon">{icon}</span>
        </button>
      {/each}

      <div class="layout-controls">
        <div
          class={`pill layout-toggle ${snapshot.layout_selector_open ? "open" : "closed"}`}
          onclick={toggleLayoutSelector}
          role="button"
          tabindex="0"
          onkeydown={(event) =>
            event.key === "Enter" && toggleLayoutSelector()}
          title="切换布局"
        >
          {snapshot.layout_symbol || "[]="}
        </div>
        {#if snapshot.layout_selector_open}
          <div class="layout-selector">
            <div
              class={`pill layout-option ${snapshot.layout_symbol === "[]=" ? "current" : ""}`}
              onclick={() => selectLayout(0, monitorId)}
              role="button"
              tabindex="0"
              onkeydown={(event) =>
                event.key === "Enter" && selectLayout(0, monitorId)}
            >
              []=
            </div>
            <div
              class={`pill layout-option ${snapshot.layout_symbol === "><>" ? "current" : ""}`}
              onclick={() => selectLayout(1, monitorId)}
              role="button"
              tabindex="0"
              onkeydown={(event) =>
                event.key === "Enter" && selectLayout(1, monitorId)}
            >
              {"><>"}
            </div>
            <div
              class={`pill layout-option ${snapshot.layout_symbol === "[M]" ? "current" : ""}`}
              onclick={() => selectLayout(2, monitorId)}
              role="button"
              tabindex="0"
              onkeydown={(event) =>
                event.key === "Enter" && selectLayout(2, monitorId)}
            >
              [M]
            </div>
          </div>
        {/if}
      </div>
    </div>

    <div class="spacer"></div>

    <div class="right-info-container">
        <div
          class="minimized-dock"
          class:is-empty={snapshot.minimized_windows.length === 0 && !snapshot.minimized_overflow}
          aria-label="Minimized windows"
        >
          <span class="minimized-divider" aria-hidden="true"></span>
          {#each snapshot.minimized_windows as item (`${wmSessionId}:${minimizedGeneration}:${item.token}`)}
            {@const label = item.title.trim() || item.app_id.trim() || "Minimized window"}
            {@const urgent = (item.flags & 2) !== 0}
            {@const previewAvailable = (item.flags & 1) !== 0}
            <button
              class:is-urgent={urgent}
              class="minimized-item"
              data-window-id={item.token}
              disabled={!snapshot.wm_available}
              onclick={(event) =>
                restoreMinimized(
                  item.token,
                  wmSessionId,
                  minimizedGeneration,
                  event.currentTarget,
                ).catch(console.error)}
              onmouseenter={(event) => {
                if (previewAvailable) {
                  beginPreview(item.token, wmSessionId, minimizedGeneration, event.currentTarget);
                }
              }}
              onmouseleave={(event) => {
                if (previewAvailable) {
                  endPreview(item.token, wmSessionId, minimizedGeneration, event.currentTarget);
                }
              }}
              title={`${label} — click to restore`}
              aria-label={`Restore ${label}`}
            >
              <span class="minimized-thumbnail" aria-hidden="true">
                <span class="minimized-traffic-lights"></span>
                <span class="minimized-initial">{minimizedInitial(item)}</span>
              </span>
              {#if urgent}<span class="minimized-urgent-dot"></span>{/if}
            </button>
          {/each}
          {#if snapshot.minimized_overflow}
            <span class="minimized-overflow" title="More minimized windows">…</span>
          {/if}
        </div>
      <div class="system-info-container">
        <div
          class={`pill usage-pill ${severity(snapshot.system_details.cpu_average)}`}
          title="CPU 平均使用率"
        >
          <span class="nf-icon">{ICON_CPU}</span>
          {`${snapshot.system_details.cpu_average.toFixed(0)}%`}
        </div>
        <div
          class={`pill usage-pill ${severity(snapshot.system_details.memory_usage_percent)}`}
          title={`内存使用: ${formatBytes(snapshot.system_details.memory_used)} / ${formatBytes(snapshot.system_details.memory_total)}`}
        >
          <span class="nf-icon">{ICON_MEM}</span>
          {`${snapshot.system_details.memory_usage_percent.toFixed(0)}%`}
        </div>
        <div
          class={`pill usage-pill ${batteryClass(snapshot)}`}
          title={batteryTitle(snapshot)}
        >
          <span class="nf-icon">
            {snapshot.battery.charging ? ICON_BAT_CHG : ICON_BAT_FULL}
          </span>
          {batteryLabel(snapshot)}
        </div>
      </div>

      <div
        class="pill brightness-pill"
        onclick={() => adjustBrightness(5)}
        onwheel={(event) => {
          event.preventDefault();
          adjustBrightness(event.deltaY < 0 ? 5 : -5);
        }}
        oncontextmenu={(event) => {
          event.preventDefault();
          adjustBrightness(-5);
        }}
        role="button"
        tabindex="0"
        onkeydown={(event) => event.key === "Enter" && adjustBrightness(5)}
        title="左键加亮 / 右键减暗 / 滚轮调节"
      >
        <span class="nf-icon">{ICON_BRIGHT}</span>
        {snapshot.brightness.percent === null
          ? "--"
          : `${snapshot.brightness.percent.toFixed(0)}%`}
      </div>

      <div
        class={`pill volume-pill ${!snapshot.audio_device || snapshot.audio_device.is_muted ? "muted" : ""}`}
        onclick={toggleMute}
        onwheel={(event) => {
          event.preventDefault();
          dispatchAction({
            action: "adjust_volume",
            delta: event.deltaY < 0 ? 5 : -5,
          }).catch(console.error);
        }}
        role="button"
        tabindex="0"
        onkeydown={(event) => event.key === "Enter" && toggleMute()}
        title={snapshot.audio_device?.name ?? "左键静音 / 滚轮调节"}
      >
        <span class="nf-icon">{volumeIcon(snapshot.audio_device)}</span>
        {snapshot.audio_device ? `${snapshot.audio_device.volume}%` : "--"}
      </div>

      <div class="shell-menu">
        <div
          class={`pill shell-pill ${snapshot.wm_available ? "" : "shell-pill-offline"}`}
          onclick={() => openShell("hub")}
          role="button"
          tabindex="0"
          onkeydown={(event) => event.key === "Enter" && openShell("hub")}
          title="JWM shell"
        >
          <span class="nf-icon">{SHELL_ROUTES[0].icon}</span>
        </div>
        {#if snapshot.wm_available}
          <div class="shell-dropdown">
            {#each SHELL_ROUTES as entry (entry.route)}
              <div
                class="shell-route"
                onclick={() => openShell(entry.route)}
                role="button"
                tabindex="0"
                onkeydown={(event) => event.key === "Enter" && openShell(entry.route)}
              >
                <span class="nf-icon">{entry.icon}</span>
                <span>{entry.label}</span>
              </div>
            {/each}
          </div>
        {/if}
      </div>

      <div
        class={`pill screenshot-pill ${isTaking ? "taking" : ""}`}
        onclick={takeScreenshot}
        role="button"
        tabindex="0"
        onkeydown={(event) => event.key === "Enter" && takeScreenshot()}
        title="截图 (jwm)"
      >
        <span class="nf-icon">{ICON_SHOT}</span>
      </div>

      <div
        class="pill time-pill"
        onclick={toggleSeconds}
        role="button"
        tabindex="0"
        onkeydown={(event) => event.key === "Enter" && toggleSeconds()}
        title={snapshot.show_seconds ? "点击隐藏秒" : "点击显示秒"}
      >
        <span class="nf-icon">{ICON_TIME}</span> {snapshot.time || "--"}
      </div>

      <div
        class="pill monitor-pill"
        title={snapshot.client_name || "显示器"}
      >
        <span class="nf-icon">{ICON_MON}</span>
        {monitorIcon(snapshot.monitor)}
      </div>

      <div class="pill scale-pill" title="Scale Factor">
        {scaleFactor === null ? "s: --" : `s: ${scaleFactor.toFixed(2)}`}
      </div>
    </div>
  </div>
{/if}
