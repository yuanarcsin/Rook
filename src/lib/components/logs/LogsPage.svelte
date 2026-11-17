<script lang="ts">
  // 日志页：左侧任务选择，右侧任务日志 / 应用事件二选一。
  import { onMount } from "svelte";
  import { app } from "$lib/state/app.svelte";
  import { LogViewState } from "$lib/state/log-view.svelte";
  import { LOG_POLL_MS, shouldPoll } from "$lib/utils/log-poll";
  import GlobalBanners from "../shell/GlobalBanners.svelte";
  import PageHeader from "../shell/PageHeader.svelte";
  import EmptyState from "../ui/EmptyState.svelte";
  import EventLogPanel from "./EventLogPanel.svelte";
  import TaskLogPanel from "./TaskLogPanel.svelte";

  let log = new LogViewState();

  onMount(() => {
    if (app.logTaskId !== null && app.tasks.some((task) => task.id === app.logTaskId)) {
      log.taskId = app.logTaskId;
    } else if (log.taskId === null) {
      log.taskId = app.tasks[0]?.id ?? null;
    }
  });

  // 从通知/详情跳过来时切换选中任务。
  $effect(() => {
    const requested = app.logTaskId;
    if (requested !== null && requested !== log.taskId && app.tasks.some((task) => task.id === requested)) {
      log.taskId = requested;
    }
  });

  // 任务被删除/重命名时收敛选择，避免读取不存在的任务。
  $effect(() => {
    if (log.taskId !== null && !app.tasks.some((task) => task.id === log.taskId)) {
      log.taskId = app.tasks[0]?.id ?? null;
    }
  });

  // 切换任务时重置文件选择，并重新列出该任务的日志文件。
  $effect(() => {
    const id = log.taskId;
    log.fileId = null;
    void log.loadFiles(id);
  });

  // 选择任务、切换行数或文件时读取一次。
  $effect(() => {
    const id = log.taskId;
    const limit = log.tailLines;
    const file = log.fileId;
    if (id === null) {
      log.view = null;
      return;
    }
    void log.load(false);
  });

  // 自动刷新只在日志页可见时运行；归档是静态文件，不轮询。
  $effect(() => {
    const id = log.taskId;
    const limit = log.tailLines;
    const file = log.fileId;
    if (id === null || file !== null || !shouldPoll(log.autoRefresh, id)) return;
    const timer = setInterval(() => void log.load(true), LOG_POLL_MS);
    return () => clearInterval(timer);
  });
</script>

<div class="page page-fill">
  <PageHeader title="日志" lead="只读；脚本日志每 2 秒刷新，应用事件可以只看状态迁移。" />
  <GlobalBanners />

  <div class="logs-bar">
    <div class="segmented" role="tablist" aria-label="日志类型">
      <button type="button" role="tab" aria-selected={app.logSegment === "task"} onclick={() => (app.logSegment = "task")}>
        脚本日志
      </button>
      <button type="button" role="tab" aria-selected={app.logSegment === "event"} onclick={() => (app.logSegment = "event")}>
        应用事件
      </button>
    </div>
    {#if app.logSegment === "task" && app.tasks.length > 0}
      <label class="logs-picker">
        <span class="logs-picker-label">脚本</span>
        <select
          value={log.taskId ?? ""}
          onchange={(event) => {
            log.taskId = event.currentTarget.value || null;
          }}
        >
          {#each app.tasks as task (task.id)}
            <option value={task.id}>{task.name || task.id}</option>
          {/each}
        </select>
      </label>
    {/if}
  </div>

  {#if app.logSegment === "task"}
    {#if app.tasks.length === 0}
      <EmptyState title="还没有脚本" text="先导入脚本或新建脚本，再查看日志。">
        {#snippet actions()}
          <button type="button" class="btn btn-primary" onclick={() => app.openImport()}>导入脚本</button>
          <button type="button" class="btn btn-ghost" onclick={() => app.openNewTask()}>新建脚本</button>
        {/snippet}
      </EmptyState>
    {:else}
      <div class="logs-layout">
        <TaskLogPanel {log} />
      </div>
    {/if}
  {:else}
    <div class="logs-layout">
      <EventLogPanel />
    </div>
  {/if}
</div>
