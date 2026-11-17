<script lang="ts">
  // 侧栏：产品名 + 三个主页面 + 脚本工作区列表 + 全局状态/退出。
  // 脚本列表与 DSH 的工作区列表同构：图标 + 名称 + 选中态，不放状态圆点。
  import { isMockMode } from "$lib/backend";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import { displayStatus, displayStatusLabel } from "$lib/utils/task-utils";
  import NotificationBell from "../notifications/NotificationBell.svelte";
  import Icon from "../ui/Icon.svelte";

  let { onquit, quitting = false }: { onquit: () => void; quitting?: boolean } = $props();

  const navItems = [
    { view: "console", label: "控制台", icon: "console" },
    { view: "logs", label: "日志", icon: "logs" },
    { view: "settings", label: "设置", icon: "settings" },
  ] as const;

  let mock = isMockMode();
  let attention = $derived(app.attentionTasks.length);
  let running = $derived(app.runningCount);
  let statusText = $derived(
    app.snapshot === null
      ? "正在连接…"
      : app.tasks.length === 0
        ? "还没有脚本"
        : app.snapshot.configError
          ? "配置需要处理"
          : attention > 0
            ? running + " 个运行中 · " + attention + " 个待处理"
            : running > 0
              ? running + " 个运行中"
              : "全部已停止",
  );

  function taskLabel(taskId: string): string {
    const task = app.tasks.find((entry) => entry.id === taskId);
    if (!task) return "";
    return (task.name || task.id) + " · " + (displayStatusLabel[displayStatus(task)] ?? displayStatus(task));
  }
</script>

<aside class="sidebar" id="app-sidebar">
  <div class="brand">
    <span class="brand-name">{BRAND.name}</span>
    <span class="brand-sub">{BRAND.tagline}</span>
  </div>

  <nav class="nav" aria-label="主导航">
    {#each navItems as item, index (item.view)}
      <button
        type="button"
        class="nav-item"
        aria-current={app.view === item.view ? "page" : undefined}
        title={item.label + "（Ctrl+" + (index + 1) + "）"}
        onclick={() => app.setView(item.view)}
      >
        <span class="nav-icon"><Icon name={item.icon} size={18} /></span>
        <span class="nav-label">{item.label}</span>
      </button>
    {/each}
  </nav>

  <div class="sidebar-section">
    <div class="sidebar-section-head">
      <span class="sidebar-section-title">脚本</span>
      <button
        type="button"
        class="icon-btn tiny"
        aria-label="新建脚本"
        title="新建脚本"
        onclick={() => app.openNewTask()}
        disabled={app.snapshot === null || app.editor.open}
      >
        <Icon name="plus" size={15} />
      </button>
    </div>
    <div class="script-list">
      {#each app.tasks as task (task.id)}
        <button
          type="button"
          class="script-item"
          aria-current={app.view === "task" && app.activeTaskId === task.id ? "page" : undefined}
          title={taskLabel(task.id)}
          onclick={() => app.openTask(task.id)}
        >
          <span class="nav-icon"><Icon name="terminal" size={16} /></span>
          <span class="nav-label truncate">{task.name || task.id}</span>
        </button>
      {/each}
      {#if app.tasks.length === 0}
        <button type="button" class="script-empty" onclick={() => app.openImport()}>
          导入第一个脚本
        </button>
      {/if}
    </div>
  </div>

  <div class="sidebar-tools">
    <NotificationBell />
  </div>

  <div class="sidebar-foot">
    {#if mock}
      <span class="demo-tag" title="浏览器演示数据：不会读写磁盘，也不会启动脚本">演示数据</span>
    {/if}
    <span class="status-line" title={statusText}>{statusText}</span>
    <button
      type="button"
      class="quit-btn"
      title="停止全部脚本并退出"
      aria-label="停止全部脚本并退出"
      onclick={onquit}
      disabled={quitting}
    >
      <span class="quit-icon"><Icon name="power" size={16} /></span>
      <span class="quit-label">{quitting ? "正在退出…" : "退出"}</span>
    </button>
  </div>
</aside>
