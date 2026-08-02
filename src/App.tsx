import { For, onCleanup, onMount, Show, createSignal } from "solid-js";
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

interface BarSnapshot {
  wm_available: boolean;
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
  | { action: "open_shell_hub"; route: ShellRoute };

const dispatchAction = (request: ActionRequest): Promise<void> =>
  invoke("dispatch_action", { request });

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
                title="截图 (Flameshot)"
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
