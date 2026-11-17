<script lang="ts">
  // 自绘标题栏：窗口关闭系统边框（tauri.conf decorations=false）后，
  // 由这一条 44px 区域承担拖动、双击最大化和窗口按钮。
  // 背景保持透明，颜色直接透出主背景渐变（不再有系统强调色染出的淡蓝色横条）。
  import { onMount } from "svelte";
  import { getCurrentWindow, type Window } from "@tauri-apps/api/window";
  import { isMockMode } from "$lib/backend";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import Icon from "../ui/Icon.svelte";

  let mock = isMockMode();
  let maximized = $state(false);
  let win: Window | null = null;

  onMount(() => {
    if (mock) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      try {
        const current = getCurrentWindow();
        win = current;
        maximized = await current.isMaximized();
        unlisten = await current.onResized(async () => {
          if (!cancelled) maximized = await current.isMaximized();
        });
      } catch {
        // 窗口 API 不可用时保留可拖动区域，按钮退化为无操作。
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  });

  function minimize() {
    void win?.minimize();
  }

  function toggleMaximize() {
    void win?.toggleMaximize();
  }

  function closeWindow() {
    void win?.close();
  }
</script>

<header class="titlebar" data-tauri-drag-region>
  <button
    type="button"
    class="titlebar-toggle"
    title={app.sidebarCollapsed ? "展开侧栏（Ctrl+B）" : "收起侧栏（Ctrl+B）"}
    aria-label={app.sidebarCollapsed ? "展开侧栏" : "收起侧栏"}
    aria-controls="app-sidebar"
    aria-expanded={!app.sidebarCollapsed}
    onclick={() => app.toggleSidebar()}
  >
    <Icon name="panel-left" size={16} />
  </button>

  <div class="titlebar-brand">
    <img class="titlebar-mark" src="/favicon.png" alt="" draggable="false" />
    <span>{BRAND.name}</span>
  </div>

  <div class="titlebar-controls">
    <button
      type="button"
      class="titlebar-btn"
      title="最小化"
      aria-label="最小化"
      onclick={minimize}
    >
      <Icon name="win-minimize" size={14} />
    </button>
    <button
      type="button"
      class="titlebar-btn"
      title={maximized ? "还原" : "最大化"}
      aria-label={maximized ? "还原" : "最大化"}
      onclick={toggleMaximize}
    >
      <Icon name={maximized ? "win-restore" : "win-maximize"} size={13} />
    </button>
    <button
      type="button"
      class="titlebar-btn close"
      title="关闭窗口（继续驻留托盘）"
      aria-label="关闭窗口"
      onclick={closeWindow}
    >
      <Icon name="win-close" size={14} />
    </button>
  </div>
</header>
