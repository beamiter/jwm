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

  let snapshot = $state<BarSnapshot | null>(null);
  let scaleFactor = $state<number | null>(null);
  let pressed = $state<number | null>(null);
  let isTaking = $state(false);

  let cancelled = false;
  let revision: number | null = null;
  let unlisten: UnlistenFn | undefined;

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
    const initialize = async () => {
      const stopListening = await listen<FrontendEnvelope>(
        "xbar-state",
        (event) => {
          if (cancelled) return;
          if (revision !== null && event.payload.revision < revision) return;
          revision = event.payload.revision;
          snapshot = event.payload.snapshot;
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
  });

  onDestroy(() => {
    cancelled = true;
    unlisten?.();
  });
</script>

{#if !snapshot}
  <div class="button-row">Loading...</div>
{:else}
  {@const monitorId = snapshot.monitor}
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
