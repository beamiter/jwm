import { For, onCleanup, onMount, Show, createEffect, createMemo, createSignal } from "solid-js";
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

const getButtonClass = (tag: TagState): string => {
  if (tag.filled) return "emoji-button state-filtered";
  if (tag.selected) return "emoji-button state-selected";
  if (tag.urgent) return "emoji-button state-urgent";
  if (tag.occupied) return "emoji-button state-occupied";
  return "emoji-button state-default";
};

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return "0B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1024)),
  );
  const size = Number((bytes / Math.pow(1024, index)).toFixed(index === 0 ? 0 : 1));
  return `${size}${units[index]}`;
};

const severity = (percent: number): string =>
  percent <= 30
    ? "usage-good"
    : percent <= 60
      ? "usage-warn"
      : percent <= 80
        ? "usage-caution"
        : "usage-danger";

const monitorIcon = (monitor: number): string => {
  if (monitor === 0) return "\u{F02DA}";
  if (monitor === 1) return "\u{F02DB}";
  return `M${monitor}`;
};

const volumeIcon = (device: AudioDeviceInfo | null): string => {
  if (!device || device.is_muted || device.volume <= 0) return ICON_VOL_MUTE;
  if (device.volume < 34) return ICON_VOL_LOW;
  if (device.volume < 67) return ICON_VOL_MID;
  return ICON_VOL_HIGH;
};

const minimizedInitial = (window: MinimizedWindow): string => {
  const label = window.app_id.trim() || window.title.trim();
  return Array.from(label)[0]?.toLocaleUpperCase() ?? "•";
};

type WindowMetrics = { x: number; y: number; scale: number };

const windowMetrics = async (): Promise<WindowMetrics | null> => {
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
};

export const projectDockGeometry = (
  rect: Pick<DOMRect, "left" | "top" | "width" | "height">,
  metrics: WindowMetrics,
): DockGeometry => {
  return {
    x: metrics.x + Math.round(rect.left * metrics.scale),
    y: metrics.y + Math.round(rect.top * metrics.scale),
    width: Math.max(0, Math.round(rect.width * metrics.scale)),
    height: Math.max(0, Math.round(rect.height * metrics.scale)),
  };
};

const physicalGeometry = (element: HTMLElement, metrics: WindowMetrics): DockGeometry =>
  projectDockGeometry(element.getBoundingClientRect(), metrics);

const restingItemGeometry = (
  item: HTMLElement,
  dock: HTMLElement,
  metrics: WindowMetrics,
): DockGeometry => {
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
};

const geometryForElement = async (element: HTMLElement): Promise<DockGeometry | undefined> => {
  const metrics = await windowMetrics();
  return metrics ? physicalGeometry(element, metrics) : undefined;
};

const restoreMinimized = async (
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  element: HTMLElement,
) => {
  const geometry = await geometryForElement(element);
  await dispatchAction({
    action: "restore_window",
    wm_session_id: wmSessionId,
    minimized_generation: minimizedGeneration,
    window_id: windowId,
    geometry,
  });
};

const previewMinimized = async (
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  visible: boolean,
  renewal: boolean,
  element: HTMLElement,
) => {
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
};

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
const previewBindingKey = (windowId: number, wmSessionId: number, minimizedGeneration: number) =>
  `${wmSessionId}:${minimizedGeneration}:${windowId}`;

const stopPreviewDelivery = (bindingKey: string, expected?: PreviewDelivery) => {
  const delivery = previewDeliveries.get(bindingKey);
  if (!delivery || (expected !== undefined && delivery !== expected)) return;
  previewDeliveries.delete(bindingKey);
  if (delivery.retryTimer !== undefined) window.clearTimeout(delivery.retryTimer);
  if (delivery.renewalTimer !== undefined) window.clearInterval(delivery.renewalTimer);
};

const previewDeliveryIsActive = (bindingKey: string, delivery: PreviewDelivery) =>
  previewDeliveries.get(bindingKey) === delivery &&
  delivery.element.isConnected &&
  delivery.element.matches(":hover");

const sendPreviewLeaveUnlessReentered = async (
  bindingKey: string,
  delivery: PreviewDelivery,
) => {
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
};

const sendAutomaticPreviewLeave = (bindingKey: string, delivery: PreviewDelivery) => {
  if (previewDeliveries.get(bindingKey) !== delivery) return;
  stopPreviewDelivery(bindingKey, delivery);
  sendPreviewLeaveUnlessReentered(bindingKey, delivery).catch(console.error);
};

const compensateDeliveredPreviewEnter = (bindingKey: string, delivery: PreviewDelivery) => {
  const current = previewDeliveries.get(bindingKey);
  if (current !== undefined && current !== delivery) return;
  if (current === delivery) stopPreviewDelivery(bindingKey, delivery);
  // Compensate a delivered ENTER that outlived its binding.
  sendPreviewLeaveUnlessReentered(bindingKey, delivery).catch(console.error);
};

function schedulePreviewEnterRetry(bindingKey: string, delivery: PreviewDelivery, error: unknown) {
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
      // A superseded binding must not turn a late ENTER success into a lease.
      if (previewDeliveryIsActive(bindingKey, delivery)) {
        startPreviewRenewal(bindingKey, delivery);
      } else {
        compensateDeliveredPreviewEnter(bindingKey, delivery);
      }
    })
    .catch((error) => schedulePreviewEnterRetry(bindingKey, delivery, error));
}

const beginPreview = (
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  element: HTMLElement,
) => {
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
};

const endPreview = (
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  element: HTMLElement,
) => {
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
};

const publishDockGeometry = async (
  dock: HTMLElement,
  wmSessionId: number,
  minimizedGeneration: number,
) => {
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
};

function MinimizedDock(props: {
  windows: MinimizedWindow[];
  overflow: boolean;
  available: boolean;
  wmSessionId: number;
  minimizedGeneration: number;
  barGeometry: DockGeometry | null;
}) {
  let dock!: HTMLDivElement;
  const geometrySignature = createMemo(
    () =>
      `${props.available}|${props.wmSessionId}|${props.minimizedGeneration}|${props.barGeometry?.x},${props.barGeometry?.y},${props.barGeometry?.width},${props.barGeometry?.height}|${props.windows
        .map((window) => window.token)
        .join(",")}|${props.overflow}`,
  );

  createEffect(() => {
    geometrySignature();
    let retryTimer: number | undefined;
    let publishInFlight = false;
    let republishRequested = false;
    let disposed = false;

    const scheduleRetry = () => {
      if (
        disposed ||
        !props.available ||
        props.wmSessionId === 0 ||
        retryTimer !== undefined ||
        !dock?.isConnected
      ) return;
      retryTimer = window.setTimeout(() => {
        retryTimer = undefined;
        publish();
      }, 100);
    };
    const publish = () => {
      if (disposed || !props.available || props.wmSessionId === 0 || !dock?.isConnected) return;
      if (retryTimer !== undefined) {
        window.clearTimeout(retryTimer);
        retryTimer = undefined;
      }
      if (publishInFlight) {
        republishRequested = true;
        return;
      }
      publishInFlight = true;
      publishDockGeometry(dock, props.wmSessionId, props.minimizedGeneration)
        .then(() => {
          publishInFlight = false;
          if (republishRequested) {
            republishRequested = false;
            publish();
          }
        })
        .catch((error) => {
          publishInFlight = false;
          republishRequested = false;
          console.error("Failed to publish minimized Dock geometry; retrying:", error);
          scheduleRetry();
        });
    };
    const frame = window.requestAnimationFrame(publish);
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(publish);
    if (dock) observer?.observe(dock);
    window.addEventListener("resize", publish);
    onCleanup(() => {
      disposed = true;
      window.cancelAnimationFrame(frame);
      if (retryTimer !== undefined) window.clearTimeout(retryTimer);
      observer?.disconnect();
      window.removeEventListener("resize", publish);
      for (const [bindingKey, delivery] of Array.from(previewDeliveries)) {
        sendAutomaticPreviewLeave(bindingKey, delivery);
      }
    });
  });

  return (
      <div
        ref={dock}
        class={`minimized-dock${props.windows.length === 0 && !props.overflow ? " is-empty" : ""}`}
        aria-label="Minimized windows"
      >
        <span class="minimized-divider" aria-hidden="true" />
        <For each={props.windows}>
          {(window) => {
            // Capture the projection identity when this keyed control is
            // created. Reading reactive props inside the event handler would
            // silently upgrade an old token to the newest generation.
            const windowId = window.token;
            const wmSessionId = props.wmSessionId;
            const minimizedGeneration = props.minimizedGeneration;
            const label = () =>
              window.title.trim() || window.app_id.trim() || "Minimized window";
            const urgent = () => (window.flags & 2) !== 0;
            const previewAvailable = () => (window.flags & 1) !== 0;
            return (
              <button
                class={`minimized-item${urgent() ? " is-urgent" : ""}`}
                data-window-id={windowId}
                disabled={!props.available}
                onClick={(event) =>
                  restoreMinimized(windowId, wmSessionId, minimizedGeneration, event.currentTarget).catch(
                    console.error,
                  )
                }
                onMouseEnter={(event) => {
                  if (previewAvailable()) {
                    beginPreview(windowId, wmSessionId, minimizedGeneration, event.currentTarget);
                  }
                }}
                onMouseLeave={(event) => {
                  if (previewAvailable()) {
                    endPreview(windowId, wmSessionId, minimizedGeneration, event.currentTarget);
                  }
                }}
                title={`${label()} — click to restore`}
                aria-label={`Restore ${label()}`}
              >
                <span class="minimized-thumbnail" aria-hidden="true">
                  <span class="minimized-traffic-lights" />
                  <span class="minimized-initial">{minimizedInitial(window)}</span>
                </span>
                <Show when={urgent()}>
                  <span class="minimized-urgent-dot" />
                </Show>
              </button>
            );
          }}
        </For>
        <Show when={props.overflow}>
          <span class="minimized-overflow" title="More minimized windows">
            …
          </span>
        </Show>
      </div>
  );
}

function App() {
  const [snapshot, setSnapshot] = createSignal<BarSnapshot | null>(null);
  const [scaleFactor, setScaleFactor] = createSignal<number | null>(null);
  const [pressed, setPressed] = createSignal<number | null>(null);
  const [isTaking, setIsTaking] = createSignal(false);

  let cancelled = false;
  let revision: number | null = null;
  let unlisten: UnlistenFn | undefined;

  onMount(() => {
    const initialize = async () => {
      setScaleFactor(await getCurrentWindow().scaleFactor());
      const stopListening = await listen<FrontendEnvelope>("xbar-state", (event) => {
        if (cancelled) return;
        if (revision !== null && event.payload.revision < revision) return;
        revision = event.payload.revision;
        snapshotBarOrigin = event.payload.snapshot.geometry;
        setSnapshot(event.payload.snapshot);
      });
      if (cancelled) {
        stopListening();
        return;
      }
      unlisten = stopListening;
      await invoke<void>("frontend_ready");
    };

    initialize().catch((error) => {
      console.error("Failed to initialize xbar Tauri bridge:", error);
    });
  });

  onCleanup(() => {
    cancelled = true;
    unlisten?.();
  });

  const selectLayout = (layoutId: number, monitor: number) =>
    dispatchAction({
      action: "set_layout_on",
      layout_id: layoutId,
      monitor_id: monitor,
    }).catch(console.error);

  // Availability is passed in: the snapshot only exists inside the <Show>
  // render prop, and the shell lives in the window manager, so a click with no
  // projection has nowhere to go.
  const openShell = (route: ShellRoute, available: boolean) => {
    if (!available) return;
    dispatchAction({ action: "open_shell_hub", route }).catch(console.error);
  };

  const takeScreenshot = async () => {
    if (isTaking()) return;
    setIsTaking(true);
    try {
      await dispatchAction({ action: "screenshot" });
    } catch (error) {
      console.error(error);
    } finally {
      window.setTimeout(() => setIsTaking(false), 500);
    }
  };

  return (
    <Show when={snapshot()} fallback={<div class="button-row">Loading...</div>}>
      {(current) => {
        const batteryPercent = () =>
          current().battery.present ? current().battery.percent : null;
        const batteryClass = () => {
          const percent = batteryPercent();
          if (percent === null) return "usage-warn";
          if (percent > 50) return "usage-good";
          if (percent > 20) return "usage-warn";
          return "usage-danger";
        };
        const optionClass = (symbol: string) =>
          `pill layout-option ${current().layout_symbol === symbol ? "current" : ""}`;

        return (
          <div class="button-row">
            <div class="buttons-container">
              <For each={TAG_ICONS}>
                {(icon, index) => {
                  const tag = () =>
                    current().tags[index()] ?? {
                      selected: false,
                      urgent: false,
                      filled: false,
                      occupied: false,
                    };
                  return (
                    <button
                      class={`${getButtonClass(tag())}${pressed() === index() ? " pressed" : ""}`}
                      onMouseDown={() => setPressed(index())}
                      onMouseUp={() => {
                        setPressed(null);
                        dispatchAction({
                          action: "view_tag_on",
                          tag_index: index(),
                          monitor_id: current().monitor,
                        }).catch(console.error);
                      }}
                      onMouseLeave={() => setPressed(null)}
                      title={`Tag ${index() + 1}`}
                    >
                      <span class="nf-icon">{icon}</span>
                    </button>
                  );
                }}
              </For>

              <div class="layout-controls">
                <div
                  class={`pill layout-toggle ${
                    current().layout_selector_open ? "open" : "closed"
                  }`}
                  onClick={() =>
                    dispatchAction({ action: "toggle_layout_selector" }).catch(console.error)
                  }
                  title="切换布局"
                >
                  {current().layout_symbol || "[]="}
                </div>
                <Show when={current().layout_selector_open}>
                  <div class="layout-selector">
                    <div
                      class={optionClass("[]=")}
                      onClick={() => selectLayout(0, current().monitor)}
                    >
                      []=
                    </div>
                    <div
                      class={optionClass("><>")}
                      onClick={() => selectLayout(1, current().monitor)}
                    >
                      {"><>"}
                    </div>
                    <div
                      class={optionClass("[M]")}
                      onClick={() => selectLayout(2, current().monitor)}
                    >
                      [M]
                    </div>
                  </div>
                </Show>
              </div>
            </div>

            <div class="spacer" />

            <div class="right-info-container">
              <MinimizedDock
                windows={current().minimized_windows}
                overflow={current().minimized_overflow}
                available={current().wm_available}
                wmSessionId={current().wm_session_id}
                minimizedGeneration={current().wm_sequence ?? 0}
                barGeometry={current().geometry}
              />
              <div class="system-info-container">
                <div
                  class={`pill usage-pill ${severity(current().system_details.cpu_average)}`}
                  title="CPU 平均使用率"
                >
                  <span class="nf-icon">{ICON_CPU}</span>{" "}
                  {current().system_details.cpu_average.toFixed(0)}%
                </div>
                <div
                  class={`pill usage-pill ${severity(
                    current().system_details.memory_usage_percent,
                  )}`}
                  title={`内存使用: ${formatBytes(
                    current().system_details.memory_used,
                  )} / ${formatBytes(current().system_details.memory_total)}`}
                >
                  <span class="nf-icon">{ICON_MEM}</span>{" "}
                  {current().system_details.memory_usage_percent.toFixed(0)}%
                </div>
                <div
                  class={`pill usage-pill ${batteryClass()}`}
                  title={
                    batteryPercent() === null
                      ? "未检测到电池"
                      : current().battery.charging
                        ? `电池充电中: ${batteryPercent()!.toFixed(1)}%`
                        : `电池电量: ${batteryPercent()!.toFixed(1)}%`
                  }
                >
                  <span class="nf-icon">
                    {current().battery.charging ? ICON_BAT_CHG : ICON_BAT_FULL}
                  </span>{" "}
                  {batteryPercent() === null ? "--" : `${batteryPercent()!.toFixed(0)}%`}
                </div>
              </div>

              <div
                class="pill brightness-pill"
                onClick={() =>
                  dispatchAction({ action: "adjust_brightness", delta: 5 }).catch(console.error)
                }
                onContextMenu={(event) => {
                  event.preventDefault();
                  dispatchAction({ action: "adjust_brightness", delta: -5 }).catch(console.error);
                }}
                onWheel={(event) => {
                  event.preventDefault();
                  dispatchAction({
                    action: "adjust_brightness",
                    delta: event.deltaY < 0 ? 5 : -5,
                  }).catch(console.error);
                }}
                title="左键加亮 / 右键减暗 / 滚轮调节"
              >
                <span class="nf-icon">{ICON_BRIGHT}</span>{" "}
                {current().brightness.percent === null
                  ? "--"
                  : `${current().brightness.percent!.toFixed(0)}%`}
              </div>

              <div
                class={`pill volume-pill ${
                  !current().audio_device || current().audio_device!.is_muted ? "muted" : ""
                }`}
                onClick={() =>
                  dispatchAction({ action: "toggle_mute" }).catch(console.error)
                }
                onWheel={(event) => {
                  event.preventDefault();
                  dispatchAction({
                    action: "adjust_volume",
                    delta: event.deltaY < 0 ? 5 : -5,
                  }).catch(console.error);
                }}
                title={current().audio_device?.name ?? "左键静音 / 滚轮调节"}
              >
                <span class="nf-icon">{volumeIcon(current().audio_device)}</span>{" "}
                {current().audio_device ? `${current().audio_device!.volume}%` : "--"}
              </div>

              <div class="shell-menu">
                <div
                  class={`pill shell-pill ${
                    current().wm_available ? "" : "shell-pill-offline"
                  }`}
                  onClick={() => openShell("hub", current().wm_available)}
                  title="JWM shell"
                >
                  <span class="nf-icon">{SHELL_ROUTES[0].icon}</span>
                </div>
                <Show when={current().wm_available}>
                  <div class="shell-dropdown">
                    <For each={SHELL_ROUTES}>
                      {(entry) => (
                        <div
                          class="shell-route"
                          onClick={() => openShell(entry.route, current().wm_available)}
                        >
                          <span class="nf-icon">{entry.icon}</span>
                          <span>{entry.label}</span>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>
              </div>

              <div
                class={`pill screenshot-pill ${isTaking() ? "taking" : ""}`}
                onClick={takeScreenshot}
                title="截图 (jwm)"
              >
                <span class="nf-icon">{ICON_SHOT}</span>
              </div>

              <div
                class="pill time-pill"
                onClick={() =>
                  dispatchAction({ action: "toggle_seconds" }).catch(console.error)
                }
                title={current().show_seconds ? "点击隐藏秒" : "点击显示秒"}
              >
                <span class="nf-icon">{ICON_TIME}</span> {current().time || "--"}
              </div>

              <div
                class="pill monitor-pill"
                title={current().client_name || "显示器"}
              >
                <span class="nf-icon">{ICON_MON}</span> {monitorIcon(current().monitor)}
              </div>

              <div class="pill scale-pill" title="Scale Factor">
                {scaleFactor() === null ? "s: --" : `s: ${scaleFactor()!.toFixed(2)}`}
              </div>
            </div>
          </div>
        );
      }}
    </Show>
  );
}

export default App;
