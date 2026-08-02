import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import "./App.css";

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

/// Pages of JWM's own shell surface, in the order the hub itself lists them.
/// Every entry is one request to the window manager: the bar renders no shell
/// content and keeps no shell state.
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

function TagButtons({ tags, monitor }: { tags: TagState[]; monitor: number }) {
  const [pressedButton, setPressedButton] = useState<number | null>(null);

  return (
    <>
      {TAG_ICONS.map((icon, index) => {
        const tag = tags[index] ?? {
          selected: false,
          urgent: false,
          filled: false,
          occupied: false,
        };
        const baseClass = getButtonClass(tag);
        const buttonClass = pressedButton === index ? `${baseClass} pressed` : baseClass;
        return (
          <button
            key={index}
            className={buttonClass}
            onMouseDown={() => setPressedButton(index)}
            onMouseUp={() => {
              setPressedButton(null);
              dispatchAction({
                action: "view_tag_on",
                tag_index: index,
                monitor_id: monitor,
              }).catch(console.error);
            }}
            onMouseLeave={() => setPressedButton(null)}
            title={`Tag ${index + 1}`}
          >
            <span className="nf-icon">{icon}</span>
          </button>
        );
      })}
    </>
  );
}

function SystemInfoDisplay({ snapshot }: { snapshot: BarSnapshot }) {
  const details = snapshot.system_details;
  const battery = snapshot.battery;
  const batteryPercent = battery.present ? battery.percent : null;
  const severity = (percent: number) =>
    percent <= 30
      ? "usage-good"
      : percent <= 60
        ? "usage-warn"
        : percent <= 80
          ? "usage-caution"
          : "usage-danger";
  const batteryClass = batteryPercent === null
    ? "usage-warn"
    : batteryPercent > 50
      ? "usage-good"
      : batteryPercent > 20
        ? "usage-warn"
        : "usage-danger";

  return (
    <div className="system-info-container">
      <div className={`pill usage-pill ${severity(details.cpu_average)}`} title="CPU 平均使用率">
        <span className="nf-icon">{ICON_CPU}</span> {details.cpu_average.toFixed(0)}%
      </div>
      <div
        className={`pill usage-pill ${severity(details.memory_usage_percent)}`}
        title={`内存使用: ${formatBytes(details.memory_used)} / ${formatBytes(details.memory_total)}`}
      >
        <span className="nf-icon">{ICON_MEM}</span> {details.memory_usage_percent.toFixed(0)}%
      </div>
      <div
        className={`pill usage-pill ${batteryClass}`}
        title={batteryPercent === null
          ? "未检测到电池"
          : battery.charging
            ? `电池充电中: ${batteryPercent.toFixed(1)}%`
            : `电池电量: ${batteryPercent.toFixed(1)}%`}
      >
        <span className="nf-icon">{battery.charging ? ICON_BAT_CHG : ICON_BAT_FULL}</span>{" "}
        {batteryPercent === null ? "--" : `${batteryPercent.toFixed(0)}%`}
      </div>
    </div>
  );
}

function VolumeControl({ device }: { device: AudioDeviceInfo | null }) {
  const muted = !device || device.is_muted;
  const onWheel = (event: React.WheelEvent) => {
    event.preventDefault();
    dispatchAction({
      action: "adjust_volume",
      delta: event.deltaY < 0 ? 5 : -5,
    }).catch(console.error);
  };

  return (
    <div
      className={muted ? "pill volume-pill muted" : "pill volume-pill"}
      onClick={() => dispatchAction({ action: "toggle_mute" }).catch(console.error)}
      onWheel={onWheel}
      title={device?.name ?? "左键静音 / 滚轮调节"}
    >
      <span className="nf-icon">{volumeIcon(device)}</span>{" "}
      {device ? `${device.volume}%` : "--"}
    </div>
  );
}

function BrightnessControl({ percent }: { percent: number | null }) {
  const adjust = (delta: number) =>
    dispatchAction({ action: "adjust_brightness", delta }).catch(console.error);
  return (
    <div
      className="pill brightness-pill"
      onClick={() => adjust(5)}
      onContextMenu={(event) => {
        event.preventDefault();
        adjust(-5);
      }}
      onWheel={(event) => {
        event.preventDefault();
        adjust(event.deltaY < 0 ? 5 : -5);
      }}
      title="左键加亮 / 右键减暗 / 滚轮调节"
    >
      <span className="nf-icon">{ICON_BRIGHT}</span>{" "}
      {percent === null ? "--" : `${percent.toFixed(0)}%`}
    </div>
  );
}

function ShellButton({ available }: { available: boolean }) {
  const open = (route: ShellRoute) => {
    // The shell lives in the window manager, so a click with no projection has
    // nowhere to go; the pill is grayed out rather than silently inert.
    if (!available) return;
    dispatchAction({ action: "open_shell_hub", route }).catch(console.error);
  };
  return (
    <div className="shell-menu">
      <div
        className={`pill shell-pill ${available ? "" : "shell-pill-offline"}`}
        onClick={() => open("hub")}
        title="JWM shell"
      >
        <span className="nf-icon">{SHELL_ROUTES[0].icon}</span>
      </div>
      {available && (
        <div className="shell-dropdown">
          {SHELL_ROUTES.map(({ route, icon, label }) => (
            <div key={route} className="shell-route" onClick={() => open(route)}>
              <span className="nf-icon">{icon}</span>
              <span>{label}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ScreenshotButton() {
  const [isTaking, setIsTaking] = useState(false);
  const takeScreenshot = async () => {
    if (isTaking) return;
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
    <div
      className={`pill screenshot-pill ${isTaking ? "taking" : ""}`}
      onClick={takeScreenshot}
      title="截图 (Flameshot)"
    >
      <span className="nf-icon">{ICON_SHOT}</span>
    </div>
  );
}

function TimeDisplay({ time, showSeconds }: { time: string; showSeconds: boolean }) {
  return (
    <div
      className="pill time-pill"
      onClick={() => dispatchAction({ action: "toggle_seconds" }).catch(console.error)}
      title={showSeconds ? "点击隐藏秒" : "点击显示秒"}
    >
      <span className="nf-icon">{ICON_TIME}</span> {time || "--"}
    </div>
  );
}

function LayoutControls({
  symbol,
  monitor,
  open,
}: {
  symbol: string;
  monitor: number;
  open: boolean;
}) {
  const selectLayout = (layoutId: number) =>
    dispatchAction({
      action: "set_layout_on",
      layout_id: layoutId,
      monitor_id: monitor,
    }).catch(console.error);
  const optionClass = (layoutSymbol: string) =>
    `pill layout-option ${symbol === layoutSymbol ? "current" : ""}`;

  return (
    <div className="layout-controls">
      <div
        className={`pill layout-toggle ${open ? "open" : "closed"}`}
        onClick={() => dispatchAction({ action: "toggle_layout_selector" }).catch(console.error)}
        title="切换布局"
      >
        {symbol || "[]="}
      </div>
      {open && (
        <div className="layout-selector">
          <div className={optionClass("[]=")} onClick={() => selectLayout(0)}>[]=</div>
          <div className={optionClass("><>")} onClick={() => selectLayout(1)}>{"><>"}</div>
          <div className={optionClass("[M]")} onClick={() => selectLayout(2)}>[M]</div>
        </div>
      )}
    </div>
  );
}

function App() {
  const [snapshot, setSnapshot] = useState<BarSnapshot | null>(null);
  const [scaleFactor, setScaleFactor] = useState<number | null>(null);
  const revision = useRef<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    const initialize = async () => {
      setScaleFactor(await getCurrentWindow().scaleFactor());
      unlisten = await listen<FrontendEnvelope>("xbar-state", (event) => {
        if (cancelled) return;
        if (revision.current !== null && event.payload.revision < revision.current) return;
        revision.current = event.payload.revision;
        setSnapshot(event.payload.snapshot);
      });
      if (cancelled) {
        unlisten();
        return;
      }
      await invoke("frontend_ready");
    };

    initialize().catch((error) => {
      console.error("Failed to initialize xbar Tauri bridge:", error);
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  if (!snapshot) return <div className="button-row">Loading...</div>;

  return (
    <div className="button-row">
      <div className="buttons-container">
        <TagButtons tags={snapshot.tags} monitor={snapshot.monitor} />
        <LayoutControls
          symbol={snapshot.layout_symbol}
          monitor={snapshot.monitor}
          open={snapshot.layout_selector_open}
        />
      </div>
      <div className="spacer" />
      <div className="right-info-container">
        <SystemInfoDisplay snapshot={snapshot} />
        <BrightnessControl percent={snapshot.brightness.percent} />
        <VolumeControl device={snapshot.audio_device} />
        <ShellButton available={snapshot.wm_available} />
        <ScreenshotButton />
        <TimeDisplay time={snapshot.time} showSeconds={snapshot.show_seconds} />
        <div className="pill monitor-pill" title={snapshot.client_name || "显示器"}>
          <span className="nf-icon">{ICON_MON}</span> {monitorIcon(snapshot.monitor)}
        </div>
        <div className="pill scale-pill" title="Scale Factor">
          {scaleFactor === null ? "s: --" : `s: ${scaleFactor.toFixed(2)}`}
        </div>
      </div>
    </div>
  );
}

export default App;
