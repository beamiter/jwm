import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

// --- 类型定义，与后端 Rust 结构体对应 ---
interface TagStatus {
  is_selected: boolean;
  is_urg: boolean;
  is_filled: boolean;
  is_occ: boolean;
}

interface MonitorInfoSnapshot {
  monitor_num: number;
  monitor_width: number;
  monitor_height: number;
  monitor_x: number;
  monitor_y: number;
  tag_status_vec: TagStatus[];
  client_name: string;
  ltsymbol: string;
}

interface SystemSnapshot {
  cpu_average: number;
  memory_used: number;
  memory_total: number;
  memory_usage_percent: number;
  battery_percent: number;
  is_charging: boolean;
}

interface AudioSnapshot {
  volume: number;
  is_muted: boolean;
  device_name: string;
  has_device: boolean;
}

interface BrightnessSnapshot {
  percent: number | null;
}

interface UiState {
  monitor_info_snapshot: MonitorInfoSnapshot | null;
  system_snapshot: SystemSnapshot | null;
  audio_snapshot: AudioSnapshot | null;
  brightness_snapshot: BrightnessSnapshot | null;
}

// --- Nerd Font 图标 ---
// 9 个 tag 使用工作流主题图标：终端 / 浏览器 / 代码 / 聊天 / 文件 / 音乐 / 视频 / 邮件 / 游戏
const TAG_ICONS = [
  "\u{F0A1E}", // 󰨞 terminal
  "\u{F0239}", // 󰈹 firefox/browser
  "\u{F0A1B}", // 󰨛 code
  "\u{F0B79}", // 󰭹 chat
  "\u{F024B}", // 󰉋 folder
  "\u{F0388}", // 󰎈 music
  "\u{F0567}", // 󰕧 video
  "\u{F01F0}", // 󰇰 mail
  "\u{F0297}", // 󰊗 gamepad
];

const ICON_CPU = "\u{F4BC}";        //  cpu chip
const ICON_MEM = "\u{F035B}";       // 󰍛 memory
const ICON_BAT_FULL = "\u{F0079}";  // 󰁹 battery full
const ICON_BAT_CHG = "\u{F0084}";   // 󰂄 charging
const ICON_VOL_HIGH = "\u{F057E}";  // 󰕾 volume high
const ICON_VOL_MID = "\u{F0580}";   // 󰖀 volume mid
const ICON_VOL_LOW = "\u{F057F}";   // 󰕿 volume low
const ICON_VOL_MUTE = "\u{F075F}";  // 󰝟 muted
const ICON_BRIGHT = "\u{F00DE}";    // 󰃞 brightness
const ICON_SHOT = "\u{F0104}";      // 󰄄 camera
const ICON_TIME = "\u{F0954}";      // 󰥔 clock
const ICON_MON = "\u{F0379}";       // 󰍹 monitor

const getButtonClass = (tagStatus: TagStatus): string => {
  if (tagStatus.is_filled) return "emoji-button state-filtered";
  if (tagStatus.is_selected) return "emoji-button state-selected";
  if (tagStatus.is_urg) return "emoji-button state-urgent";
  if (tagStatus.is_occ) return "emoji-button state-occupied";
  return "emoji-button state-default";
};

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return "0B";
  const UNITS = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  const size = parseFloat((bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1));
  return `${size}${UNITS[i]}`;
};

function parseLtSymbol(lts: string | undefined) {
  if (!lts) return { symbol: "[]=", scale: undefined };
  const symbolMatch = lts.match(/^(\S+)/);
  const scaleMatch = lts.match(/s:\s*([0-9.]+)/i);
  const symbol = symbolMatch ? symbolMatch[1] : "[]=";
  const scale = scaleMatch ? parseFloat(scaleMatch[1]) : undefined;
  return { symbol, scale };
}

function monitorIcon(num: number) {
  if (num === 0) return "\u{F02DA}"; // 󰋚 M0 indicator (numbered)
  if (num === 1) return "\u{F02DB}";
  return `M${num}`;
}

function volumeIcon(snap: AudioSnapshot | null): string {
  if (!snap || !snap.has_device) return ICON_VOL_MUTE;
  if (snap.is_muted) return ICON_VOL_MUTE;
  if (snap.volume <= 0) return ICON_VOL_MUTE;
  if (snap.volume < 34) return ICON_VOL_LOW;
  if (snap.volume < 67) return ICON_VOL_MID;
  return ICON_VOL_HIGH;
}

// --- 子组件 ---

const TagButtons = (
  { tags, monitorNum }: { tags: TagStatus[]; monitorNum: number },
) => {
  const [pressedButton, setPressedButton] = useState<number | null>(null);

  const handlePress = (index: number) => setPressedButton(index);
  const handleRelease = (index: number) => {
    setPressedButton(null);
    invoke("send_tag_command", {
      tagIndex: index,
      isView: true,
      monitorId: monitorNum,
    }).catch((e) => console.error(e));
  };

  return (
    <>
      {TAG_ICONS.map((icon, i) => {
        const tagStatus = tags[i] || {
          is_selected: false,
          is_urg: false,
          is_filled: false,
          is_occ: false,
        };
        const baseClass = getButtonClass(tagStatus);
        const isPressed = pressedButton === i;
        const buttonClass = isPressed ? `${baseClass} pressed` : baseClass;

        return (
          <button
            key={i}
            className={buttonClass}
            onMouseDown={() => handlePress(i)}
            onMouseUp={() => handleRelease(i)}
            onMouseLeave={() => setPressedButton(null)}
            title={`Tag ${i + 1}`}
          >
            <span className="nf-icon">{icon}</span>
          </button>
        );
      })}
    </>
  );
};

const SystemInfoDisplay = (
  { snapshot }: { snapshot: SystemSnapshot | null },
) => {
  if (!snapshot) {
    return (
      <div className="system-info-container">
        <div className="pill usage-pill usage-warn">
          <span className="nf-icon">{ICON_CPU}</span> --%
        </div>
        <div className="pill usage-pill usage-warn">
          <span className="nf-icon">{ICON_MEM}</span> --%
        </div>
        <div className="pill usage-pill usage-warn">
          <span className="nf-icon">{ICON_BAT_FULL}</span> --%
        </div>
      </div>
    );
  }

  const sev = (p: number) =>
    p <= 30
      ? "usage-good"
      : p <= 60
      ? "usage-warn"
      : p <= 80
      ? "usage-caution"
      : "usage-danger";

  const cpuClass = sev(snapshot.cpu_average);
  const memClass = sev(snapshot.memory_usage_percent);
  const battClass = snapshot.battery_percent > 50
    ? "usage-good"
    : snapshot.battery_percent > 20
    ? "usage-warn"
    : "usage-danger";
  const batteryIcon = snapshot.is_charging ? ICON_BAT_CHG : ICON_BAT_FULL;

  return (
    <div className="system-info-container">
      <div className={`pill usage-pill ${cpuClass}`} title="CPU 平均使用率">
        <span className="nf-icon">{ICON_CPU}</span>{" "}
        {`${snapshot.cpu_average.toFixed(0)}%`}
      </div>
      <div
        className={`pill usage-pill ${memClass}`}
        title={`内存使用: ${formatBytes(snapshot.memory_used)} / ${
          formatBytes(snapshot.memory_total)
        }`}
      >
        <span className="nf-icon">{ICON_MEM}</span>{" "}
        {`${snapshot.memory_usage_percent.toFixed(0)}%`}
      </div>
      <div
        className={`pill usage-pill ${battClass}`}
        title={snapshot.is_charging
          ? `电池充电中: ${snapshot.battery_percent.toFixed(1)}%`
          : `电池电量: ${snapshot.battery_percent.toFixed(1)}%`}
      >
        <span className="nf-icon">{batteryIcon}</span>{" "}
        {`${snapshot.battery_percent.toFixed(0)}%`}
      </div>
    </div>
  );
};

const VolumeControl = ({ snapshot }: { snapshot: AudioSnapshot | null }) => {
  const onClick = () => {
    invoke("toggle_mute").catch((e) => console.error(e));
  };
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const delta = e.deltaY < 0 ? 5 : -5;
    invoke("adjust_volume", { delta }).catch((e2) => console.error(e2));
  };

  const muted = !snapshot || snapshot.is_muted || !snapshot.has_device;
  const vol = snapshot?.volume ?? 0;
  const cls = muted ? "pill volume-pill muted" : "pill volume-pill";
  const label = snapshot?.has_device ? `${vol}%` : "--";

  return (
    <div
      className={cls}
      onClick={onClick}
      onWheel={onWheel}
      title="左键静音 / 滚轮调节"
    >
      <span className="nf-icon">{volumeIcon(snapshot)}</span> {label}
    </div>
  );
};

const BrightnessControl = (
  { snapshot }: { snapshot: BrightnessSnapshot | null },
) => {
  const onClick = () => {
    invoke("adjust_brightness", { delta: 5 }).catch((e) => console.error(e));
  };
  const onContextMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    invoke("adjust_brightness", { delta: -5 }).catch((e2) => console.error(e2));
  };
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const delta = e.deltaY < 0 ? 5 : -5;
    invoke("adjust_brightness", { delta }).catch((e2) => console.error(e2));
  };

  const pct = snapshot?.percent;
  const label = typeof pct === "number" ? `${pct}%` : "--";

  return (
    <div
      className="pill brightness-pill"
      onClick={onClick}
      onWheel={onWheel}
      onContextMenu={onContextMenu}
      title="左键加亮 / 右键减暗 / 滚轮调节"
    >
      <span className="nf-icon">{ICON_BRIGHT}</span> {label}
    </div>
  );
};

const ScreenshotButton = () => {
  const [isTaking, setIsTaking] = useState(false);

  const handleClick = async () => {
    if (isTaking) return;
    setIsTaking(true);
    try {
      await invoke("take_screenshot");
    } catch (e) {
      console.error(e);
    } finally {
      setTimeout(() => setIsTaking(false), 500);
    }
  };

  return (
    <div
      className={`pill screenshot-pill ${isTaking ? "taking" : ""}`}
      onClick={handleClick}
      title="截图 (Flameshot)"
    >
      <span className="nf-icon">{ICON_SHOT}</span>
    </div>
  );
};

const TimeDisplay = () => {
  const [showSeconds, setShowSeconds] = useState(true);
  const [time, setTime] = useState(new Date());

  useEffect(() => {
    const interval = setInterval(
      () => setTime(new Date()),
      showSeconds ? 1000 : 60000,
    );
    return () => clearInterval(interval);
  }, [showSeconds]);

  const pad = (n: number) => n.toString().padStart(2, "0");
  const formatted = useMemo(() => {
    const d = time;
    const ts = `${pad(d.getHours())}:${pad(d.getMinutes())}${
      showSeconds ? `:${pad(d.getSeconds())}` : ""
    }`;
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${ts}`;
  }, [time, showSeconds]);

  return (
    <div
      className="pill time-pill"
      onClick={() => setShowSeconds(!showSeconds)}
      title="点击切换秒显示"
    >
      <span className="nf-icon">{ICON_TIME}</span> {formatted}
    </div>
  );
};

const LayoutControls = ({
  ltsymbol,
  monitorNum,
}: {
  ltsymbol: string;
  monitorNum: number;
}) => {
  const [open, setOpen] = useState(false);
  const { symbol } = parseLtSymbol(ltsymbol);

  const toggleClass = `pill layout-toggle ${open ? "open" : "closed"}`;
  const optClass = (sym: string) =>
    `pill layout-option ${symbol === sym ? "current" : ""}`;

  const onSelect = (idx: number) => {
    setOpen(false);
    invoke("send_layout_command", {
      layoutIndex: idx,
      monitorId: monitorNum,
    }).catch((e) => console.error(e));
  };

  return (
    <div className="layout-controls">
      <div className={toggleClass} onClick={() => setOpen(!open)} title="切换布局">
        {symbol}
      </div>
      {open && (
        <div className="layout-selector">
          <div className={optClass("[]=")} onClick={() => onSelect(0)}>
            []=
          </div>
          <div className={optClass("><>")} onClick={() => onSelect(1)}>
            <>{"><>"}</>
          </div>
          <div className={optClass("[M]")} onClick={() => onSelect(2)}>
            [M]
          </div>
        </div>
      )}
    </div>
  );
};

// --- 主 App 组件 ---
function App() {
  const [appState, setAppState] = useState<UiState>({
    monitor_info_snapshot: null,
    system_snapshot: null,
    audio_snapshot: null,
    brightness_snapshot: null,
  });

  const mounted = useRef(true);

  useEffect(() => {
    console.log("Tauri React frontend has loaded.");
    mounted.current = true;
    let cancelled = false;
    const unlisteners: Array<() => void> = [];

    const registerListeners = async () => {
      const keep = (unlisten: () => void) => {
        if (cancelled) {
          unlisten();
          return false;
        }
        unlisteners.push(unlisten);
        return true;
      };

      const unlistenMonitor = await listen<MonitorInfoSnapshot | null>(
        "monitor-update",
        (event) => {
          if (!mounted.current) return;
          setAppState((prev) => ({
            ...prev,
            monitor_info_snapshot: event.payload,
          }));
        },
      );
      if (!keep(unlistenMonitor)) return;

      const unlistenSystem = await listen<SystemSnapshot>(
        "system-update",
        (event) => {
          if (!mounted.current) return;
          setAppState((prev) => ({ ...prev, system_snapshot: event.payload }));
        },
      );
      if (!keep(unlistenSystem)) return;

      const unlistenAudio = await listen<AudioSnapshot>("audio-update", (event) => {
        if (!mounted.current) return;
        setAppState((prev) => ({ ...prev, audio_snapshot: event.payload }));
      });
      if (!keep(unlistenAudio)) return;

      const unlistenBrightness = await listen<BrightnessSnapshot>(
        "brightness-update",
        (event) => {
          if (!mounted.current) return;
          setAppState((prev) => ({
            ...prev,
            brightness_snapshot: event.payload,
          }));
        },
      );
      if (!keep(unlistenBrightness)) return;

      await invoke<void>("frontend_ready");
    };

    void registerListeners().catch((error) => {
      console.error("Failed to initialize Tauri event bridge:", error);
    });

    return () => {
      cancelled = true;
      mounted.current = false;
      unlisteners.splice(0).forEach((unlisten) => unlisten());
    };
  }, []);

  if (!appState.monitor_info_snapshot) {
    return <div className="button-row">Loading...</div>;
  }

  const mis = appState.monitor_info_snapshot;
  const { scale } = parseLtSymbol(mis.ltsymbol);

  return (
    <div className="button-row">
      <div className="buttons-container">
        <TagButtons tags={mis.tag_status_vec} monitorNum={mis.monitor_num} />
        <LayoutControls ltsymbol={mis.ltsymbol} monitorNum={mis.monitor_num} />
      </div>

      <div className="spacer" />

      <div className="right-info-container">
        <SystemInfoDisplay snapshot={appState.system_snapshot} />
        <BrightnessControl snapshot={appState.brightness_snapshot} />
        <VolumeControl snapshot={appState.audio_snapshot} />
        <ScreenshotButton />
        <TimeDisplay />
        <div className="pill monitor-pill" title="显示器">
          <span className="nf-icon">{ICON_MON}</span> {monitorIcon(mis.monitor_num)}
        </div>
        <div className="pill scale-pill" title="Scale Factor">
          {scale !== undefined ? `s: ${scale.toFixed(2)}` : "s: --"}
        </div>
      </div>
    </div>
  );
}

export default App;
