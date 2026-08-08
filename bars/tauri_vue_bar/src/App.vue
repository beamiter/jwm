<template>
  <!-- 加载态 -->
  <div v-if="!monitorSnapshot" class="button-row">Loading...</div>

  <!-- 主界面 -->
  <div v-else class="button-row">
    <div class="buttons-container">
      <!-- Tag Buttons -->
      <button
        v-for="(icon, i) in TAG_ICONS"
        :key="i"
        :class="buttonClass(i)"
        @mousedown="pressedButton = i"
        @mouseup="onTagRelease(i)"
        @mouseleave="pressedButton = null"
        :title="`Tag ${i + 1}`"
      >
        <span class="nf-icon">{{ icon }}</span>
      </button>

      <!-- 布局切换 -->
      <div class="layout-controls">
        <div
          :class="['pill', 'layout-toggle', layoutOpen ? 'open' : 'closed']"
          @click="onLayoutToggle"
          title="切换布局"
        >
          {{ currentSymbol }}
        </div>
        <div v-if="layoutOpen" class="layout-selector">
          <div
            :class="['pill', 'layout-option', currentSymbol === '[]=' ? 'current' : '']"
            @click="onLayoutSelect(0)"
          >
            []=
          </div>
          <div
            :class="['pill', 'layout-option', currentSymbol === '><>' ? 'current' : '']"
            @click="onLayoutSelect(1)"
          >
            &gt;&lt;&gt;
          </div>
          <div
            :class="['pill', 'layout-option', currentSymbol === '[M]' ? 'current' : '']"
            @click="onLayoutSelect(2)"
          >
            [M]
          </div>
        </div>
      </div>
    </div>

    <div class="spacer"></div>

    <div class="right-info-container">
      <div
        class="minimized-dock"
        :class="{ 'is-empty': !monitorSnapshot.minimized_windows.length && !monitorSnapshot.minimized_overflow }"
        aria-label="Minimized windows"
      >
        <span class="minimized-divider" aria-hidden="true"></span>
        <button
          v-for="item in minimizedDockItems"
          :key="`${item.wm_session_id}:${item.minimized_generation}:${item.token}`"
          class="minimized-item"
          :class="{ 'is-urgent': (item.flags & 2) !== 0 }"
          :data-window-id="item.token"
          :disabled="!wmAvailable"
          :title="`${item.title.trim() || item.app_id.trim() || 'Minimized window'} — click to restore`"
          :aria-label="`Restore ${item.title.trim() || item.app_id.trim() || 'Minimized window'}`"
          @click="onRestoreWindow(item.token, item.wm_session_id, item.minimized_generation, $event)"
          @mouseenter="(item.flags & 1) !== 0 && beginPreviewWindow(item.token, item.wm_session_id, item.minimized_generation, $event)"
          @mouseleave="(item.flags & 1) !== 0 && endPreviewWindow(item.token, item.wm_session_id, item.minimized_generation, $event)"
        >
          <span class="minimized-thumbnail" aria-hidden="true">
            <span class="minimized-traffic-lights"></span>
            <span class="minimized-initial">{{ minimizedInitial(item) }}</span>
          </span>
          <span v-if="(item.flags & 2) !== 0" class="minimized-urgent-dot"></span>
        </button>
        <span
          v-if="monitorSnapshot.minimized_overflow"
          class="minimized-overflow"
          title="More minimized windows"
        >…</span>
      </div>

      <!-- 系统信息 -->
      <template v-if="systemSnapshot">
        <div class="system-info-container">
          <div class="pill usage-pill" :class="cpuClass" title="CPU 平均使用率">
            <span class="nf-icon">{{ ICON_CPU }}</span>
            {{ Math.round(systemSnapshot.cpu_average) }}%
          </div>
          <div
            class="pill usage-pill"
            :class="memClass"
            :title="`内存使用: ${formatBytes(systemSnapshot.memory_used)} / ${formatBytes(systemSnapshot.memory_total)}`"
          >
            <span class="nf-icon">{{ ICON_MEM }}</span>
            {{ Math.round(systemSnapshot.memory_usage_percent) }}%
          </div>
          <div
            class="pill usage-pill"
            :class="battClass"
            :title="batteryTitle"
          >
            <span class="nf-icon">{{ batteryCharging ? ICON_BAT_CHG : ICON_BAT_FULL }}</span>
            {{ batteryLabel }}
          </div>
        </div>
      </template>
      <template v-else>
        <div class="system-info-container">
          <div class="pill usage-pill usage-warn">
            <span class="nf-icon">{{ ICON_CPU }}</span> --%
          </div>
          <div class="pill usage-pill usage-warn">
            <span class="nf-icon">{{ ICON_MEM }}</span> --%
          </div>
          <div class="pill usage-pill usage-warn">
            <span class="nf-icon">{{ ICON_BAT_FULL }}</span> --%
          </div>
        </div>
      </template>

      <!-- 亮度 -->
      <div
        class="pill brightness-pill"
        @click="onBrightnessClick"
        @wheel.prevent="onBrightnessWheel"
        @contextmenu.prevent="onBrightnessRight"
        title="左键加亮 / 右键减暗 / 滚轮调节"
      >
        <span class="nf-icon">{{ ICON_BRIGHT }}</span> {{ brightnessLabel }}
      </div>

      <!-- 音量 -->
      <div
        :class="['pill', 'volume-pill', volumeMuted ? 'muted' : '']"
        @click="onToggleMute"
        @wheel.prevent="onVolumeWheel"
        title="左键静音 / 滚轮调节"
      >
        <span class="nf-icon">{{ volumeIconChar }}</span> {{ volumeLabel }}
      </div>

      <!-- JWM shell 入口 -->
      <div class="shell-menu">
        <div
          class="pill shell-pill"
          :class="{ 'shell-pill-offline': !wmAvailable }"
          @click="onOpenShell('hub')"
          title="JWM shell"
        >
          <span class="nf-icon">{{ SHELL_ROUTES[0].icon }}</span>
        </div>
        <div v-if="wmAvailable" class="shell-dropdown">
          <div
            v-for="entry in SHELL_ROUTES"
            :key="entry.route"
            class="shell-route"
            @click="onOpenShell(entry.route)"
          >
            <span class="nf-icon">{{ entry.icon }}</span>
            <span>{{ entry.label }}</span>
          </div>
        </div>
      </div>

      <!-- 截图按钮 -->
      <div
        class="pill screenshot-pill"
        :class="{ taking: isTaking }"
        @click="onScreenshot"
        title="截图 (jwm)"
      >
        <span class="nf-icon">{{ ICON_SHOT }}</span>
      </div>

      <!-- 时间 -->
      <div
        class="pill time-pill"
        @click="onToggleSeconds"
        :title="showSeconds ? '点击隐藏秒' : '点击显示秒'"
      >
        <span class="nf-icon">{{ ICON_TIME }}</span> {{ formattedTime }}
      </div>

      <!-- 显示器/缩放 -->
      <div class="pill monitor-pill" title="显示器">
        <span class="nf-icon">{{ ICON_MON }}</span> {{ monitorIcon(monitorNum) }}
      </div>
      <div class="pill scale-pill" title="Scale Factor">
        s: {{ scaleText }}
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';

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
  memory_used: number;
  memory_total: number;
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

interface BoundMinimizedWindow extends MinimizedWindow {
  wm_session_id: number;
  minimized_generation: number;
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
  | 'hub'
  | 'applications'
  | 'notifications'
  | 'clipboard'
  | 'calendar'
  | 'wallpaper';

type ActionRequest =
  | { action: 'view_tag_on'; tag_index: number; monitor_id: number }
  | { action: 'toggle_layout_selector' }
  | { action: 'set_layout_on'; layout_id: number; monitor_id: number }
  | { action: 'toggle_seconds' }
  | { action: 'toggle_mute' }
  | { action: 'adjust_volume'; delta: number }
  | { action: 'adjust_brightness'; delta: number }
  | { action: 'screenshot' }
  | {
      action: 'restore_window';
      wm_session_id: number;
      minimized_generation: number;
      window_id: number;
      geometry?: DockGeometry;
    }
  | {
      action: 'preview_window';
      wm_session_id: number;
      minimized_generation: number;
      window_id: number;
      visible: boolean;
      renewal: boolean;
      geometry?: DockGeometry;
    }
  | {
      action: 'set_dock_geometry';
      wm_session_id: number;
      minimized_generation: number;
      window_id?: number | null;
      geometry?: DockGeometry;
    }
  | { action: 'open_shell_hub'; route: ShellRoute };

const dispatchAction = (request: ActionRequest): Promise<void> =>
  invoke('dispatch_action', { request });

let snapshotBarOrigin: Pick<DockGeometry, 'x' | 'y'> | null = null;
let currentWmSessionId = 0;
let currentMinimizedGeneration = 0;
let currentWmAvailable = false;

// --- Nerd Font 图标 ---
const TAG_ICONS = [
  '\u{F0A1E}', // terminal
  '\u{F0239}', // firefox
  '\u{F0A1B}', // code
  '\u{F0B79}', // chat
  '\u{F024B}', // folder
  '\u{F0388}', // music
  '\u{F0567}', // video
  '\u{F01F0}', // mail
  '\u{F0297}', // gamepad
];

const ICON_CPU = '\u{F4BC}';
const ICON_MEM = '\u{F035B}';
const ICON_BAT_FULL = '\u{F0079}';
const ICON_BAT_CHG = '\u{F0084}';
const ICON_VOL_HIGH = '\u{F057E}';
const ICON_VOL_MID = '\u{F0580}';
const ICON_VOL_LOW = '\u{F057F}';
const ICON_VOL_MUTE = '\u{F075F}';
const ICON_BRIGHT = '\u{F00DE}';
const ICON_SHOT = '\u{F0104}';

// Pages of JWM's own shell surface, in the order the hub itself lists them.
// Every entry is one request to the window manager: the bar renders no shell
// content and keeps no shell state.
const SHELL_ROUTES: { route: ShellRoute; icon: string; label: string }[] = [
  { route: 'hub', icon: '\u{F0F2A}', label: 'Shell Hub' },
  { route: 'applications', icon: '\u{F0D22}', label: 'Applications' },
  { route: 'notifications', icon: '\u{F009A}', label: 'Notifications' },
  { route: 'clipboard', icon: '\u{F0192}', label: 'Clipboard' },
  { route: 'calendar', icon: '\u{F00ED}', label: 'Calendar' },
  { route: 'wallpaper', icon: '\u{F02E9}', label: 'Wallpaper' },
];
const ICON_TIME = '\u{F0954}';
const ICON_MON = '\u{F0379}';

// --- 帮助函数 ---
const getButtonClass = (tag: TagState): string => {
  if (tag.filled) return 'emoji-button state-filtered';
  if (tag.selected) return 'emoji-button state-selected';
  if (tag.urgent) return 'emoji-button state-urgent';
  if (tag.occupied) return 'emoji-button state-occupied';
  return 'emoji-button state-default';
};

const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const index = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1024)),
  );
  const size = Number((bytes / Math.pow(1024, index)).toFixed(index === 0 ? 0 : 1));
  return `${size}${units[index]}`;
};

function monitorIcon(num: number) {
  if (num === 0) return '\u{F02DA}';
  if (num === 1) return '\u{F02DB}';
  return `M${num}`;
}

function minimizedInitial(item: MinimizedWindow): string {
  const label = item.app_id.trim() || item.title.trim();
  return Array.from(label)[0]?.toLocaleUpperCase() ?? '•';
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
    console.error('Failed to resolve dock geometry:', error);
    return null;
  }
}

function projectDockGeometry(
  rect: Pick<DOMRect, 'left' | 'top' | 'width' | 'height'>,
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

async function geometryForElement(element: HTMLElement): Promise<DockGeometry | undefined> {
  const metrics = await windowMetrics();
  return metrics ? physicalGeometry(element, metrics) : undefined;
}

async function publishDockGeometry(
  dock: HTMLElement,
  wmSessionId: number,
  minimizedGeneration: number,
) {
  const metrics = await windowMetrics();
  if (!dock.isConnected) return;
  if (!metrics) throw new Error('Dock window metrics are temporarily unavailable');
  await dispatchAction({
    action: 'set_dock_geometry',
    wm_session_id: wmSessionId,
    minimized_generation: minimizedGeneration,
    window_id: null,
    geometry: physicalGeometry(dock, metrics),
  });
  for (const item of dock.querySelectorAll<HTMLElement>('[data-window-id]')) {
    const windowId = Number(item.dataset.windowId);
    if (!Number.isFinite(windowId)) continue;
    await dispatchAction({
      action: 'set_dock_geometry',
      wm_session_id: wmSessionId,
      minimized_generation: minimizedGeneration,
      window_id: windowId,
      geometry: restingItemGeometry(item, dock, metrics),
    });
  }
}

// --- 响应式状态 ---
const snapshot = ref<BarSnapshot | null>(null);
const scaleFactor = ref<number | null>(null);
const pressedButton = ref<number | null>(null);
const isTaking = ref(false);

let cancelled = false;
let revision: number | null = null;
let unlisten: UnlistenFn | undefined;
let dockGeometrySignature = '';
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
const previewBindingKey = (
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
): string => `${wmSessionId}:${minimizedGeneration}:${windowId}`;

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
      console.error('Failed to publish minimized Dock geometry; retrying:', error);
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
    dockGeometrySignature = '';
    return;
  }
  const minimizedGeneration = current.wm_sequence ?? 0;
  const signature = `${current.wm_session_id}|${minimizedGeneration}|${current.geometry?.x},${current.geometry?.y},${current.geometry?.width},${current.geometry?.height}|${current.minimized_windows.map((item) => item.token).join(',')}|${current.minimized_overflow}`;
  if (signature === dockGeometrySignature) return;
  dockGeometrySignature = signature;
  window.requestAnimationFrame(() => {
    const dock = document.querySelector<HTMLElement>('.minimized-dock');
    if (!dock) return;
    requestDockGeometryPublish(dock, current.wm_session_id, minimizedGeneration);
    dockResizeObserver?.disconnect();
    if (typeof ResizeObserver !== 'undefined') {
      dockResizeObserver = new ResizeObserver(() =>
        requestDockGeometryPublish(dock, current.wm_session_id, minimizedGeneration),
      );
      dockResizeObserver.observe(dock);
    }
  });
}

function handleDockResize() {
  const dock = document.querySelector<HTMLElement>('.minimized-dock');
  if (dock && snapshot.value) {
    requestDockGeometryPublish(
      dock,
      snapshot.value.wm_session_id,
      snapshot.value.wm_sequence ?? 0,
    );
  }
}

// --- 事件监听（Tauri） ---
onMounted(() => {
  window.addEventListener('resize', handleDockResize);
  (async () => {
    const stopListening = await listen<FrontendEnvelope>('xbar-state', (event) => {
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
      snapshot.value = event.payload.snapshot;
      scheduleDockGeometry(event.payload.snapshot);
    });
    if (cancelled) {
      stopListening();
      return;
    }
    unlisten = stopListening;

    try {
      scaleFactor.value = await getCurrentWindow().scaleFactor();
    } catch (error) {
      console.error('Failed to query the Tauri window scale factor:', error);
    }
    await invoke<void>('frontend_ready');
  })().catch((error) => {
    console.error('Failed to initialize xbar Tauri bridge:', error);
  });
});

onBeforeUnmount(() => {
  cancelled = true;
  unlisten?.();
  window.removeEventListener('resize', handleDockResize);
  dockResizeObserver?.disconnect();
  cancelDockGeometryRetry();
  stopAllPreviewDeliveries();
});

// --- 计算属性 ---
const monitorSnapshot = computed(() => snapshot.value);
const minimizedDockItems = computed<BoundMinimizedWindow[]>(() => {
  const current = snapshot.value;
  if (!current) return [];
  const minimizedGeneration = current.wm_sequence ?? 0;
  return current.minimized_windows.map((item) => ({
    ...item,
    wm_session_id: current.wm_session_id,
    minimized_generation: minimizedGeneration,
  }));
});
const wmAvailable = computed(() => snapshot.value?.wm_available ?? false);
const systemSnapshot = computed(() => snapshot.value?.system_details ?? null);
const audioSnapshot = computed(() => snapshot.value?.audio_device ?? null);
const monitorNum = computed(() => snapshot.value?.monitor ?? 0);
const currentSymbol = computed(() => snapshot.value?.layout_symbol || '[]=');
const layoutOpen = computed(() => snapshot.value?.layout_selector_open ?? false);
const showSeconds = computed(() => snapshot.value?.show_seconds ?? true);
const formattedTime = computed(() => snapshot.value?.time || '--');
const scaleText = computed(() =>
  scaleFactor.value === null ? '--' : scaleFactor.value.toFixed(2),
);

const cpuClass = computed(() => {
  if (!systemSnapshot.value) return 'usage-warn';
  const p = systemSnapshot.value.cpu_average;
  return p <= 30 ? 'usage-good' : p <= 60 ? 'usage-warn' : p <= 80 ? 'usage-caution' : 'usage-danger';
});

const memClass = computed(() => {
  if (!systemSnapshot.value) return 'usage-warn';
  const p = systemSnapshot.value.memory_usage_percent;
  return p <= 30 ? 'usage-good' : p <= 60 ? 'usage-warn' : p <= 80 ? 'usage-caution' : 'usage-danger';
});

const batteryPercent = computed(() => {
  const battery = snapshot.value?.battery;
  return battery?.present ? battery.percent : null;
});

const battClass = computed(() => {
  const percent = batteryPercent.value;
  if (percent === null) return 'usage-warn';
  return percent > 50 ? 'usage-good' : percent > 20 ? 'usage-warn' : 'usage-danger';
});

const batteryCharging = computed(() => snapshot.value?.battery.charging ?? false);

const batteryTitle = computed(() => {
  const percent = batteryPercent.value;
  if (percent === null) return '未检测到电池';
  return batteryCharging.value
    ? `电池充电中: ${percent.toFixed(1)}%`
    : `电池电量: ${percent.toFixed(1)}%`;
});

const batteryLabel = computed(() => {
  const percent = batteryPercent.value;
  return percent === null ? '--' : `${percent.toFixed(0)}%`;
});

const volumeMuted = computed(() => {
  const s = audioSnapshot.value;
  return !s || s.is_muted;
});

const volumeIconChar = computed(() => {
  const s = audioSnapshot.value;
  if (!s) return ICON_VOL_MUTE;
  if (s.is_muted) return ICON_VOL_MUTE;
  if (s.volume <= 0) return ICON_VOL_MUTE;
  if (s.volume < 34) return ICON_VOL_LOW;
  if (s.volume < 67) return ICON_VOL_MID;
  return ICON_VOL_HIGH;
});

const volumeLabel = computed(() => {
  const s = audioSnapshot.value;
  if (!s) return '--';
  return `${s.volume}%`;
});

const brightnessLabel = computed(() => {
  const percent = snapshot.value?.brightness.percent;
  return typeof percent === 'number' ? `${percent.toFixed(0)}%` : '--';
});

// --- 事件处理 ---
function buttonClass(i: number) {
  const tag = snapshot.value?.tags[i] ?? {
    selected: false,
    urgent: false,
    filled: false,
    occupied: false,
  };
  const baseClass = getButtonClass(tag);
  const isPressed = pressedButton.value === i;
  return isPressed ? `${baseClass} pressed` : baseClass;
}

async function onTagRelease(index: number) {
  pressedButton.value = null;
  try {
    await dispatchAction({
      action: 'view_tag_on',
      tag_index: index,
      monitor_id: monitorNum.value,
    });
  } catch (error) {
    console.error('view_tag_on error:', error);
  }
}

async function onLayoutToggle() {
  try {
    await dispatchAction({ action: 'toggle_layout_selector' });
  } catch (error) {
    console.error('toggle_layout_selector error:', error);
  }
}

async function onLayoutSelect(layoutId: number) {
  try {
    await dispatchAction({
      action: 'set_layout_on',
      layout_id: layoutId,
      monitor_id: monitorNum.value,
    });
  } catch (error) {
    console.error('set_layout_on error:', error);
  }
}

function onOpenShell(route: ShellRoute) {
  // The shell lives in the window manager, so a click with no projection has
  // nowhere to go; the pill is grayed out rather than silently inert.
  if (!wmAvailable.value) return;
  dispatchAction({ action: 'open_shell_hub', route }).catch(console.error);
}

async function onRestoreWindow(
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  event: MouseEvent,
) {
  if (!(event.currentTarget instanceof HTMLElement)) return;
  const geometry = await geometryForElement(event.currentTarget);
  await dispatchAction({
    action: 'restore_window',
    wm_session_id: wmSessionId,
    minimized_generation: minimizedGeneration,
    window_id: windowId,
    geometry,
  });
}

async function sendPreviewWindow(
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  visible: boolean,
  renewal: boolean,
  element: HTMLElement,
) {
  const geometry = await geometryForElement(element);
  if (visible && !element.matches(':hover')) return;
  await dispatchAction({
    action: 'preview_window',
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
    delivery.element.matches(':hover')
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
    action: 'preview_window',
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
  console.error('Failed to enter minimized preview; retrying:', error);
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
    sendPreviewWindow(
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
  sendPreviewWindow(
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

function beginPreviewWindow(
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  event: MouseEvent,
) {
  if (!(event.currentTarget instanceof HTMLElement)) return;
  const element = event.currentTarget;
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

function endPreviewWindow(
  windowId: number,
  wmSessionId: number,
  minimizedGeneration: number,
  event: MouseEvent,
) {
  if (!(event.currentTarget instanceof HTMLElement)) return;
  const bindingKey = previewBindingKey(windowId, wmSessionId, minimizedGeneration);
  const delivery = previewDeliveries.get(bindingKey);
  if (delivery) {
    sendAutomaticPreviewLeave(bindingKey, delivery);
  } else {
    sendPreviewLeaveUnlessReentered(bindingKey, {
      windowId,
      wmSessionId,
      minimizedGeneration,
      element: event.currentTarget,
    }).catch(console.error);
  }
}

async function onScreenshot() {
  if (isTaking.value) return;
  isTaking.value = true;
  try {
    await dispatchAction({ action: 'screenshot' });
  } catch (error) {
    console.error('screenshot error:', error);
  } finally {
    window.setTimeout(() => (isTaking.value = false), 500);
  }
}

async function onToggleMute() {
  try {
    await dispatchAction({ action: 'toggle_mute' });
  } catch (error) {
    console.error('toggle_mute error:', error);
  }
}

async function onVolumeWheel(e: WheelEvent) {
  const delta = e.deltaY < 0 ? 5 : -5;
  try {
    await dispatchAction({ action: 'adjust_volume', delta });
  } catch (error) {
    console.error('adjust_volume error:', error);
  }
}

async function onBrightnessClick() {
  try {
    await dispatchAction({ action: 'adjust_brightness', delta: 5 });
  } catch (error) {
    console.error('adjust_brightness error:', error);
  }
}

async function onBrightnessRight() {
  try {
    await dispatchAction({ action: 'adjust_brightness', delta: -5 });
  } catch (error) {
    console.error('adjust_brightness error:', error);
  }
}

async function onBrightnessWheel(e: WheelEvent) {
  const delta = e.deltaY < 0 ? 5 : -5;
  try {
    await dispatchAction({ action: 'adjust_brightness', delta });
  } catch (error) {
    console.error('adjust_brightness error:', error);
  }
}

async function onToggleSeconds() {
  try {
    await dispatchAction({ action: 'toggle_seconds' });
  } catch (error) {
    console.error('toggle_seconds error:', error);
  }
}
</script>

<style>
/* 重置所有默认样式 */
* {
  margin: 0;
  padding: 0;
  box-sizing: border-box;
}

html,
body {
  margin: 0;
  padding: 0;
  height: 40px !important;
  overflow: hidden;
  font-family:
    "Symbols Nerd Font",
    "JetBrainsMono Nerd Font",
    "FiraCode Nerd Font",
    "Hack Nerd Font",
    system-ui,
    -apple-system,
    BlinkMacSystemFont,
    "Segoe UI",
    Roboto,
    sans-serif;
  background: transparent;
}

/* Nerd Font 图标统一字体回退 */
.nf-icon {
  font-family:
    "Symbols Nerd Font",
    "JetBrainsMono Nerd Font",
    "FiraCode Nerd Font",
    "Hack Nerd Font",
    "Symbols Nerd Font Mono",
    monospace;
  font-size: 15px;
  line-height: 1;
  display: inline-block;
  vertical-align: middle;
  width: 1.2em;
  text-align: center;
}

.pill .nf-icon {
  margin-right: 6px;
}

.emoji-button .nf-icon {
  margin: 0;
  font-size: 18px;
  width: auto;
}

#main,
#app {
  margin: 0;
  padding: 0;
  height: 40px !important;
  overflow: hidden;
}

.button-row {
  display: flex;
  flex-direction: row;
  align-items: center;
  justify-content: space-between;
  margin: 0;
  padding: 1px 6px;
  gap: 8px;
  width: 100vw;
  height: 40px;
  min-height: 40px;
  max-height: 40px;
  background: rgba(255, 255, 255, 0.95);
  box-shadow: 0 0 10px rgba(0, 0, 0, 0.1);
  position: relative;
  overflow: visible;
  box-sizing: border-box;
}

.buttons-container {
  display: flex;
  flex-direction: row;
  align-items: center;
  gap: 8px;
  flex-shrink: 1;
  flex-grow: 0;
  min-width: 0;
  overflow: visible;
  padding: 2px 0;
}

.right-info-container {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-shrink: 0;
  flex-grow: 0;
  margin-left: auto;
}

.system-info-container {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}

.system-metric {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 3px 6px;
  background: rgba(248, 249, 250, 0.8);
  border-radius: 6px;
  border: 1px solid rgba(222, 226, 230, 0.8);
  transition: all 0.2s ease;
  cursor: default;
  user-select: none;
}

.system-metric:hover {
  background: rgba(233, 236, 239, 0.9);
  border-color: rgba(173, 181, 189, 0.8);
  transform: scale(1.02);
  box-shadow: 0 2px 4px rgba(0, 0, 0, 0.1);
}

.metric-icon {
  font-size: 14px;
  line-height: 1;
}

.metric-value {
  font-family:
    "JetBrains Mono", "Fira Code", "Cascadia Code", "SF Mono", Consolas,
    monospace;
  font-size: 13px;
  font-weight: 600;
  min-width: 40px;
  text-align: right;
}

.layout-symbol {
  color: #000000;
  font-size: 14px;
  padding: 4px 8px;
  background-color: rgba(255, 255, 255, 0.1);
  border-radius: 4px;
  border: 1px solid rgba(255, 255, 255, 0.2);
  min-width: 20px;
  text-align: center;
  margin-left: 8px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

/* ==================== 按钮基础样式 ==================== */

.emoji-button {
  width: 38px;
  height: 32px;
  min-width: 38px;
  min-height: 32px;
  max-width: 38px;
  max-height: 32px;
  font-size: 18px;
  border: 1px solid transparent;
  border-radius: 6px;
  background: transparent;
  cursor: pointer;
  transition: all 0.2s ease;
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
  user-select: none;
  flex-shrink: 0;
  overflow: hidden;
}

.emoji-button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
  filter: grayscale(50%);
}

.emoji-button > * {
  position: relative;
  z-index: 2;
}

.emoji-button.state-default {
  background: #ffffff;
  border-color: #dee2e6;
}

.emoji-button.state-default:hover:not(:disabled):not(.pressed):not(:active) {
  background: #f8f9fa;
  border-color: #adb5bd;
  transform: scale(1.02);
  box-shadow: 0 2px 6px rgba(0, 0, 0, 0.15);
}

/* 各索引位置的颜色状态 */
.emoji-button:nth-child(1).state-occupied { background: rgba(255, 107, 107, 0.3) !important; border-color: rgba(255, 107, 107, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(1).state-selected { background: rgba(255, 107, 107, 0.7) !important; border-color: rgba(255, 107, 107, 0.9) !important; color: white !important; }
.emoji-button:nth-child(1).state-filtered { background: rgba(255, 107, 107, 1) !important; border-color: rgba(255, 107, 107, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(255, 107, 107, 0.4); }

.emoji-button:nth-child(2).state-occupied { background: rgba(78, 205, 196, 0.3) !important; border-color: rgba(78, 205, 196, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(2).state-selected { background: rgba(78, 205, 196, 0.7) !important; border-color: rgba(78, 205, 196, 0.9) !important; color: white !important; }
.emoji-button:nth-child(2).state-filtered { background: rgba(78, 205, 196, 1) !important; border-color: rgba(78, 205, 196, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(78, 205, 196, 0.4); }

.emoji-button:nth-child(3).state-occupied { background: rgba(69, 183, 209, 0.3) !important; border-color: rgba(69, 183, 209, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(3).state-selected { background: rgba(69, 183, 209, 0.7) !important; border-color: rgba(69, 183, 209, 0.9) !important; color: white !important; }
.emoji-button:nth-child(3).state-filtered { background: rgba(69, 183, 209, 1) !important; border-color: rgba(69, 183, 209, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(69, 183, 209, 0.4); }

.emoji-button:nth-child(4).state-occupied { background: rgba(150, 206, 180, 0.3) !important; border-color: rgba(150, 206, 180, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(4).state-selected { background: rgba(150, 206, 180, 0.7) !important; border-color: rgba(150, 206, 180, 0.9) !important; color: white !important; }
.emoji-button:nth-child(4).state-filtered { background: rgba(150, 206, 180, 1) !important; border-color: rgba(150, 206, 180, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(150, 206, 180, 0.4); }

.emoji-button:nth-child(5).state-occupied { background: rgba(254, 202, 87, 0.3) !important; border-color: rgba(254, 202, 87, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(5).state-selected { background: rgba(254, 202, 87, 0.7) !important; border-color: rgba(254, 202, 87, 0.9) !important; color: #333 !important; }
.emoji-button:nth-child(5).state-filtered { background: rgba(254, 202, 87, 1) !important; border-color: rgba(254, 202, 87, 1) !important; color: #333 !important; box-shadow: 0 2px 8px rgba(254, 202, 87, 0.4); }

.emoji-button:nth-child(6).state-occupied { background: rgba(255, 159, 243, 0.3) !important; border-color: rgba(255, 159, 243, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(6).state-selected { background: rgba(255, 159, 243, 0.7) !important; border-color: rgba(255, 159, 243, 0.9) !important; color: white !important; }
.emoji-button:nth-child(6).state-filtered { background: rgba(255, 159, 243, 1) !important; border-color: rgba(255, 159, 243, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(255, 159, 243, 0.4); }

.emoji-button:nth-child(7).state-occupied { background: rgba(84, 160, 255, 0.3) !important; border-color: rgba(84, 160, 255, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(7).state-selected { background: rgba(84, 160, 255, 0.7) !important; border-color: rgba(84, 160, 255, 0.9) !important; color: white !important; }
.emoji-button:nth-child(7).state-filtered { background: rgba(84, 160, 255, 1) !important; border-color: rgba(84, 160, 255, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(84, 160, 255, 0.4); }

.emoji-button:nth-child(8).state-occupied { background: rgba(95, 39, 205, 0.3) !important; border-color: rgba(95, 39, 205, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(8).state-selected { background: rgba(95, 39, 205, 0.7) !important; border-color: rgba(95, 39, 205, 0.9) !important; color: white !important; }
.emoji-button:nth-child(8).state-filtered { background: rgba(95, 39, 205, 1) !important; border-color: rgba(95, 39, 205, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(95, 39, 205, 0.4); }

.emoji-button:nth-child(9).state-occupied { background: rgba(0, 210, 211, 0.3) !important; border-color: rgba(0, 210, 211, 0.6) !important; color: #333 !important; }
.emoji-button:nth-child(9).state-selected { background: rgba(0, 210, 211, 0.7) !important; border-color: rgba(0, 210, 211, 0.9) !important; color: white !important; }
.emoji-button:nth-child(9).state-filtered { background: rgba(0, 210, 211, 1) !important; border-color: rgba(0, 210, 211, 1) !important; color: white !important; box-shadow: 0 2px 8px rgba(0, 210, 211, 0.4); }

.emoji-button.state-urgent {
  background: linear-gradient(135deg, #dc3545, #c82333) !important;
  border-color: #bd2130 !important;
  color: white !important;
}

.emoji-button.state-urgent::after {
  content: "U";
  position: absolute;
  top: -3px;
  right: -3px;
  background: #ffc107;
  border-radius: 50%;
  width: 12px;
  height: 12px;
  border: 1px solid white;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.3);
  font-size: 8px;
  display: flex;
  align-items: center;
  justify-content: center;
  color: #000;
  font-weight: bold;
}

.emoji-button.state-filtered::after { content: "●"; position: absolute; top: 2px; right: 2px; color: rgba(255, 255, 255, 0.9); font-size: 10px; text-shadow: 0 1px 2px rgba(0, 0, 0, 0.5); font-weight: bold; }
.emoji-button.state-selected::after { content: "◆"; position: absolute; top: 2px; right: 2px; color: rgba(255, 255, 255, 0.9); font-size: 8px; text-shadow: 0 1px 2px rgba(0, 0, 0, 0.5); font-weight: bold; }
.emoji-button:nth-child(5).state-selected::after,
.emoji-button:nth-child(5).state-filtered::after { color: rgba(51, 51, 51, 0.8); text-shadow: 0 1px 1px rgba(255, 255, 255, 0.3); }
.emoji-button.state-occupied::after { content: "○"; position: absolute; top: 2px; right: 2px; color: rgba(51, 51, 51, 0.7); font-size: 8px; text-shadow: 0 1px 1px rgba(255, 255, 255, 0.3); font-weight: bold; }

/* 按下效果 */
.emoji-button::before {
  content: "";
  position: absolute;
  top: 50%;
  left: 50%;
  width: 0;
  height: 0;
  border-radius: 50%;
  background: radial-gradient(circle, rgba(255, 255, 255, 0.6) 0%, rgba(255, 255, 255, 0) 70%);
  transform: translate(-50%, -50%);
  opacity: 0;
  pointer-events: none;
  z-index: 1;
  transition: all 0.3s ease;
}

.emoji-button.pressed,
.emoji-button:active {
  transform: scale(0.92) !important;
  box-shadow:
    inset 0 2px 6px rgba(0, 0, 0, 0.3),
    0 1px 2px rgba(0, 0, 0, 0.2) !important;
  transition: all 0.1s ease !important;
}

.emoji-button.state-default.pressed,
.emoji-button.state-default:active {
  background: #dee2e6 !important;
  border-color: #6c757d !important;
}

.emoji-button.state-occupied.pressed,
.emoji-button.state-selected.pressed,
.emoji-button.state-filtered.pressed {
  opacity: 0.8;
  transform: scale(0.92) !important;
  box-shadow: inset 0 2px 6px rgba(0, 0, 0, 0.3) !important;
}

.emoji-button:hover:not(.pressed):not(:active) {
  transform: scale(1.05);
  transition: all 0.2s ease;
}

.emoji-button:disabled.pressed,
.emoji-button:disabled:active {
  transform: none !important;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1) !important;
  background: #f8f9fa !important;
}

.emoji-button:disabled::before {
  display: none;
}

@media (hover: none) {
  .emoji-button:hover { transform: none; }
  .emoji-button.pressed,
  .emoji-button:active { transform: scale(0.95) !important; }
}

/* 通用 pill 样式 */
.pill {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: 12px;
  padding: 4px 10px;
  font-size: 14px;
  line-height: 1;
  border: 1px solid transparent;
  transition: all 120ms ease-in-out;
  white-space: nowrap;
}

.system-info-container {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.usage-pill {
  color: #fff;
  border-width: 1px;
}

.usage-good { background: rgba(31, 191, 81, 0.90); border-color: #1fbf51; }
.usage-warn { background: rgba(244, 194, 13, 0.90); border-color: #f4c20d; color: #000; }
.usage-caution { background: rgba(255, 140, 26, 0.90); border-color: #ff8c1a; }
.usage-danger { background: rgba(229, 57, 53, 0.90); border-color: #e53935; }

.layout-controls {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  margin-left: 6px;
}

.layout-toggle { cursor: pointer; color: #fff; }
.layout-toggle.open { background: rgba(60, 179, 113, 0.85); border-color: #3cb371; }
.layout-toggle.closed { background: rgba(211, 84, 0, 0.85); border-color: #d35400; }
.layout-toggle:hover { filter: brightness(1.05); border-width: 2px; }

.layout-selector { display: inline-flex; align-items: center; gap: 6px; }
.layout-option { cursor: pointer; color: #fff; background: rgba(65, 105, 225, 0.85); border-color: #4169e1; }
.layout-option.current { background: rgba(60, 179, 113, 0.9); border-color: #3cb371; border-width: 2px; }
.layout-option:hover { filter: brightness(1.05); border-width: 2px; }

/* ── JWM shell entry ─────────────────────────────────────────────────────── */
/* The dropdown is CSS-only: hovering the pill reveals the page list, so the
   bar needs no open/closed state of its own for a menu the window manager
   immediately covers with its own grabbed surface. */
.shell-menu {
  position: relative;
  display: inline-flex;
}

.shell-pill {
  cursor: pointer;
  color: #fff;
  background: rgba(124, 108, 255, 0.9);
  border-color: #7c6cff;
}
.shell-pill:hover {
  background: rgba(150, 136, 255, 0.95);
  border-color: #9688ff;
}

/* Grayed rather than hidden: the shell lives in the window manager, so an
   unreachable one has to look unreachable instead of swallowing the click. */
.shell-pill-offline {
  cursor: default;
  background: rgba(85, 91, 102, 0.7);
  border-color: #555b66;
  color: rgba(255, 255, 255, 0.55);
}

.shell-dropdown {
  display: none;
  position: absolute;
  top: calc(100% + 6px);
  right: 0;
  z-index: 20;
  min-width: 170px;
  flex-direction: column;
  gap: 2px;
  padding: 6px;
  border-radius: 10px;
  border: 1px solid rgba(124, 108, 255, 0.45);
  background: rgba(20, 26, 36, 0.97);
  box-shadow: 0 8px 20px rgba(0, 0, 0, 0.45);
}

.shell-menu:hover .shell-dropdown {
  display: flex;
}

.shell-route {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 8px;
  border-radius: 6px;
  font-size: 13px;
  color: #e9eef5;
  cursor: pointer;
  white-space: nowrap;
}
.shell-route:hover {
  background: rgba(124, 108, 255, 0.28);
}

.screenshot-pill { cursor: pointer; color: #fff; background: rgba(0, 204, 204, 0.9); border-color: #00cccc; }
.screenshot-pill:hover { background: rgba(255, 136, 0, 0.95); border-color: #ff8800; }

.time-pill { color: #fff; background: rgba(77, 163, 255, 0.90); border-color: #4da3ff; cursor: pointer; }
.monitor-pill { color: #fff; background: rgba(155, 89, 182, 0.90); border-color: #9b59b6; }
.scale-pill { color: #fff; background: rgba(120, 120, 120, 0.88); border-color: #777; }

/* 音量 pill */
.volume-pill {
  cursor: pointer;
  color: #fff;
  background: rgba(20, 184, 166, 0.90);
  border-color: #14b8a6;
  user-select: none;
}
.volume-pill:hover { filter: brightness(1.08); border-width: 2px; }
.volume-pill.muted { background: rgba(120, 120, 120, 0.85); border-color: #888; color: #eee; }

/* 亮度 pill */
.brightness-pill {
  cursor: pointer;
  color: #1f2937;
  background: rgba(253, 224, 71, 0.92);
  border-color: #facc15;
  user-select: none;
}
.brightness-pill:hover { filter: brightness(1.05); border-width: 2px; }

.spacer {
  flex: 1 1 auto;
}

/* macOS-inspired minimized-window shelf (shared across Tauri web frontends). */
:root {
  --dock-shelf-bg: rgba(246, 247, 250, 0.78);
  --dock-shelf-border: rgba(79, 88, 105, 0.24);
  --dock-item-border: rgba(17, 24, 39, 0.34);
  --dock-item-shadow: 0 2px 5px rgba(15, 23, 42, 0.22);
  --dock-motion: 180ms cubic-bezier(0.2, 0.8, 0.2, 1);
  --dock-urgent: #ff453a;
}

.minimized-dock {
  position: relative;
  display: flex;
  align-items: flex-end;
  gap: 3px;
  height: 34px;
  padding: 3px 5px;
  flex: 0 0 auto;
  border: 1px solid var(--dock-shelf-border);
  border-radius: 10px;
  background: var(--dock-shelf-bg);
  box-shadow: inset 0 1px rgba(255, 255, 255, 0.78), 0 2px 7px rgba(15, 23, 42, 0.12);
  backdrop-filter: blur(16px) saturate(1.3);
  -webkit-backdrop-filter: blur(16px) saturate(1.3);
  isolation: isolate;
}
.minimized-divider { width: 1px; height: 22px; margin: 0 3px 1px 0; background: rgba(60, 67, 79, 0.3); box-shadow: 1px 0 rgba(255, 255, 255, 0.78); }
.minimized-item {
  position: relative;
  width: 30px;
  height: 20px;
  min-width: 30px;
  margin: 0;
  padding: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  cursor: pointer;
  transform: translateY(0) scale(1);
  transform-origin: 50% 100%;
  transition: transform var(--dock-motion), filter var(--dock-motion);
  will-change: transform;
  z-index: 1;
}
.minimized-item:disabled { cursor: default; opacity: 0.48; filter: grayscale(0.45); }
.minimized-thumbnail { position: absolute; inset: 0; display: grid; place-items: center; overflow: hidden; border: 1px solid var(--dock-item-border); border-radius: 6px; color: rgba(255, 255, 255, 0.96); background: linear-gradient(145deg, #4077a8 0%, #27364e 55%, #18202f 100%); box-shadow: inset 0 1px rgba(255, 255, 255, 0.42), var(--dock-item-shadow); }
.minimized-item:nth-of-type(6n + 2) .minimized-thumbnail { background: linear-gradient(145deg, #9d5c63, #513548 60%, #252238); }
.minimized-item:nth-of-type(6n + 3) .minimized-thumbnail { background: linear-gradient(145deg, #4f8b72, #285c5a 60%, #18333c); }
.minimized-item:nth-of-type(6n + 4) .minimized-thumbnail { background: linear-gradient(145deg, #a66c43, #6e4438 60%, #30262b); }
.minimized-item:nth-of-type(6n + 5) .minimized-thumbnail { background: linear-gradient(145deg, #6e64ad, #424176 60%, #24283e); }
.minimized-item:nth-of-type(6n) .minimized-thumbnail { background: linear-gradient(145deg, #4d88a1, #315269 60%, #222c3b); }
.minimized-traffic-lights { position: absolute; top: 3px; left: 4px; width: 3px; height: 3px; border-radius: 50%; background: #ff5f57; box-shadow: 5px 0 #febc2e, 10px 0 #28c840; opacity: 0.92; }
.minimized-initial { margin-top: 4px; font: 700 13px/1 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; text-shadow: 0 1px 2px rgba(0, 0, 0, 0.45); }
.minimized-item:has(+ .minimized-item + .minimized-item:hover),
.minimized-item:hover + .minimized-item + .minimized-item { transform: translateY(-1px) scale(1.08); z-index: 2; }
.minimized-item:has(+ .minimized-item:hover),
.minimized-item:hover + .minimized-item { transform: translateY(-1px) scale(1.25); z-index: 3; }
.minimized-item:hover:not(:disabled) { transform: translateY(-1px) scale(1.55); filter: brightness(1.08) saturate(1.08); z-index: 4; }
.minimized-urgent-dot { position: absolute; right: -2px; bottom: -2px; width: 7px; height: 7px; border: 1px solid white; border-radius: 50%; background: var(--dock-urgent); box-shadow: 0 0 0 2px rgba(255, 69, 58, 0.2); animation: minimized-urgent-pulse 1.35s ease-in-out infinite; z-index: 5; }
.minimized-overflow { display: grid; place-items: center; width: 18px; height: 24px; color: #4b5563; font: 700 15px/1 system-ui, sans-serif; user-select: none; }
@keyframes minimized-urgent-pulse { 50% { box-shadow: 0 0 0 4px rgba(255, 69, 58, 0.08); } }
@media (prefers-reduced-motion: reduce) {
  .minimized-item { transition: none; }
  .minimized-urgent-dot { animation: none; }
}
.minimized-dock.is-empty { padding-right: 0; border-color: transparent; background: transparent; box-shadow: none; backdrop-filter: none; -webkit-backdrop-filter: none; }
</style>
