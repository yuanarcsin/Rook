<script lang="ts">
  // 应用外壳：侧栏 + 页面切换 + 全局浮层（详情/编辑器/导入/配置/通知/退出）。
  // 这里不放业务逻辑，页面各自负责自己的内容。
  import { onMount } from "svelte";
  import "../app.css";
  import { quitApp } from "$lib/api";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import { hasModal } from "$lib/utils/modal-stack";
  import { errorMessage } from "$lib/utils/task-utils";
  import Sidebar from "$lib/components/shell/Sidebar.svelte";
  import AppTitlebar from "$lib/components/shell/AppTitlebar.svelte";
  import PageHeader from "$lib/components/shell/PageHeader.svelte";
  import ConsolePage from "$lib/components/overview/ConsolePage.svelte";
  import TaskPage from "$lib/components/tasks/TaskPage.svelte";
  import LogsPage from "$lib/components/logs/LogsPage.svelte";
  import SettingsPage from "$lib/components/settings/SettingsPage.svelte";
  import ConfigEditor from "$lib/components/settings/ConfigEditor.svelte";
  import TaskEditor from "$lib/components/tasks/TaskEditor.svelte";
  import PluginImport from "$lib/components/tasks/PluginImport.svelte";
  import Toast from "$lib/components/notifications/Toast.svelte";
  import ConfirmDialog from "$lib/components/ui/ConfirmDialog.svelte";

  let confirmQuit = $state(false);
  let quitting = $state(false);
  let quitError = $state<string | null>(null);

  onMount(() => {
    app.initChrome();
    // 窄窗自动收起侧栏；放宽后恢复用户手动选择的状态。
    const narrow = window.matchMedia("(max-width: 900px)");
    const syncSidebar = () => app.syncSidebarForWidth(narrow.matches);
    syncSidebar();
    narrow.addEventListener("change", syncSidebar);
    let dispose: (() => void) | undefined;
    let cancelled = false;
    void app.init().then((cleanup) => {
      if (cancelled) {
        cleanup();
      } else {
        dispose = cleanup;
      }
    });
    return () => {
      narrow.removeEventListener("change", syncSidebar);
      cancelled = true;
      dispose?.();
    };
  });

  /** 退出：confirm_exit=true 时先确认；false 时直接走。 */
  function requestQuit() {
    if (quitting) return;
    if (app.snapshot?.confirmExit === false) {
      void doQuit();
      return;
    }
    confirmQuit = true;
  }

  async function doQuit() {
    confirmQuit = false;
    quitting = true;
    quitError = null;
    try {
      await quitApp();
    } catch (failure) {
      quitError = errorMessage(failure);
      quitting = false;
    }
  }

  /** Ctrl+1..3 切换页面、Ctrl+B 收起/展开侧栏：输入框/弹窗内不抢焦点。 */
  function onWindowKeydown(event: KeyboardEvent) {
    if (!event.ctrlKey || event.altKey || event.metaKey || event.shiftKey) return;
    if (hasModal()) return;
    const target = event.target;
    if (target instanceof HTMLElement) {
      if (target.isContentEditable) return;
      const tag = target.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
    }
    if (event.key === "b") {
      event.preventDefault();
      app.toggleSidebar();
      return;
    }
    const views = ["console", "logs", "settings"] as const;
    const index = ["1", "2", "3"].indexOf(event.key);
    if (index < 0) return;
    event.preventDefault();
    app.setView(views[index]);
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div class="window">
  <AppTitlebar />

  <div class="app-shell" class:sidebar-collapsed={app.sidebarCollapsed}>
    <Sidebar onquit={requestQuit} {quitting} />

    <main class="main">
      {#if app.snapshot === null}
        {#if app.loadError}
          <div class="page">
            <PageHeader title="连不上后端" lead={BRAND.name + " 没有读到配置或快照。"} />
            <div class="callout callout-danger">
              <div class="callout-title">读取失败</div>
              <p>{app.loadError}</p>
              <div class="callout-actions">
                <button type="button" class="btn btn-sm" onclick={() => void app.load()}>重试</button>
              </div>
            </div>
          </div>
        {:else}
          <div class="page">
            <PageHeader title="控制台" lead="正在读取配置…" />
            <div class="skeleton-list" aria-label="正在加载">
              <div class="skeleton"></div>
              <div class="skeleton"></div>
              <div class="skeleton"></div>
            </div>
          </div>
        {/if}
      {:else if app.view === "console"}
        <ConsolePage />
      {:else if app.view === "task" && app.activeTask}
        {#key app.activeTask.id}
          <TaskPage task={app.activeTask} />
        {/key}
      {:else if app.view === "logs"}
        <LogsPage />
      {:else if app.view === "settings"}
        <SettingsPage />
      {:else}
        <ConsolePage />
      {/if}
    </main>
  </div>
</div>

{#if app.editor.open}
  {#key app.editor.session}
    <TaskEditor />
  {/key}
{/if}

{#if app.importOpen}
  <PluginImport />
{/if}

{#if app.configOpen}
  <ConfigEditor />
{/if}

{#if app.toastItems.length > 0 && !app.editor.open && !app.importOpen && !app.configOpen}
  <div class="toast-stack">
    {#each app.toastItems as item (item.key)}
      <Toast {item} />
    {/each}
  </div>
{/if}

{#if quitError}
  <div class="dock-alert callout callout-danger" role="alert">
    <div class="callout-title">退出请求失败</div>
    <p>{quitError}</p>
    <button type="button" class="banner-close" aria-label="关闭" onclick={() => (quitError = null)}>×</button>
  </div>
{/if}

{#if confirmQuit}
  <ConfirmDialog
    title={"退出 " + BRAND.name}
    message="将停止全部任务并退出，最多等待退出预算（默认 8 秒）。未保存的编辑请先保存。"
    confirmLabel="停止全部并退出"
    tone="danger"
    busy={quitting}
    onconfirm={() => void doQuit()}
    oncancel={() => (confirmQuit = false)}
  />
{/if}
