<script lang="ts">
  // 控制台：总控台。只放三件事——整体状态、脚本卡片、下一步动作。
  // 详情/编辑/时段/插件都拆到每个脚本自己的页面（侧栏“脚本”列表进入）。
  import { onMount } from "svelte";
  import { app } from "$lib/state/app.svelte";
  import { moveTask, runBatch } from "$lib/services/tasks";
  import { errorMessage } from "$lib/utils/task-utils";
  import { formatWhen } from "$lib/utils/format";
  import GlobalBanners from "../shell/GlobalBanners.svelte";
  import PageHeader from "../shell/PageHeader.svelte";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";
  import EmptyState from "../ui/EmptyState.svelte";
  import Menu, { type MenuItem } from "../ui/Menu.svelte";
  import Overlay from "../ui/Overlay.svelte";
  import TaskCard from "./TaskCard.svelte";

  let onlyAttention = $state(false);
  let pendingBatch = $state<"start" | "stop" | null>(null);
  let batchBusy = $state(false);
  let batchReport = $state<string | null>(null);
  let reorderOpen = $state(false);
  let reorderBusy = $state(false);
  let reorderError = $state<string | null>(null);
  let now = $state(Date.now());

  onMount(() => {
    const timer = setInterval(() => {
      now = Date.now();
    }, 30_000);
    return () => clearInterval(timer);
  });

  let nextStart = $derived.by(() => {
    let best: { taskName: string; at: number } | null = null;
    for (const task of app.tasks) {
      if (!task.schedule.enabled) continue;
      const at = task.schedule.nextStartAt;
      if (at === null) continue;
      if (best === null || at < best.at) {
        best = { taskName: task.name || task.id, at };
      }
    }
    return best;
  });

  let lead = $derived.by(() => {
    if (app.snapshot === null) return "正在读取配置…";
    if (app.tasks.length === 0) return "还没有脚本。导入一份清单，或手动新建。";
    const parts: string[] = [];
    if (app.runningCount > 0) parts.push(app.runningCount + " 个脚本在运行");
    if (app.attentionTasks.length > 0) parts.push(app.attentionTasks.length + " 个待处理");
    if (parts.length === 0) parts.push("全部已停止");
    if (nextStart !== null) {
      parts.push("下次 " + formatWhen(nextStart.at, now) + " 自动启动");
    }
    return parts.join(" · ");
  });

  let visible = $derived(onlyAttention ? app.attentionTasks : app.tasks);

  let bulkMenu = $derived<MenuItem[]>([
    {
      label: "全部启动",
      hint: "跳过已在运行的脚本",
      disabled: batchBusy,
      onselect: () => (pendingBatch = "start"),
    },
    {
      label: "全部停止",
      hint: "按各脚本的退出预算执行",
      disabled: batchBusy,
      onselect: () => (pendingBatch = "stop"),
    },
    {
      label: "调整顺序",
      hint: "上/下移并写入配置",
      onselect: () => (reorderOpen = true),
    },
  ]);

  let batchMessage = $derived.by(() => {
    const kind = pendingBatch;
    if (!kind) return "";
    const verb = kind === "start" ? "启动" : "停止";
    const targets = app.tasks.filter((task) => task.allowedActions?.[kind] === true);
    if (targets.length === 0) return "当前没有需要" + verb + "的脚本。";
    const skipped = app.tasks.length - targets.length;
    const skippedNote = skipped > 0 ? skipped + " 个脚本当前状态不需要" + verb + "，会自动跳过。" : "";
    const tail =
      kind === "stop"
        ? "停止会按各脚本的退出预算执行，可能需要几秒。"
        : "启动会按配置逐个拉起，失败项不影响其余脚本。";
    return "将" + verb + " " + targets.length + " 个脚本（同时最多 3 个），" + tail + skippedNote;
  });

  async function runPendingBatch() {
    const kind = pendingBatch;
    pendingBatch = null;
    if (!kind) return;
    batchBusy = true;
    batchReport = null;
    try {
      const report = await runBatch(kind);
      batchReport = report.message;
    } catch (failure) {
      batchReport = errorMessage(failure);
    } finally {
      batchBusy = false;
    }
  }

  async function move(from: number, to: number) {
    if (reorderBusy || to < 0 || to >= app.tasks.length) return;
    reorderBusy = true;
    reorderError = null;
    try {
      await moveTask(from, to);
    } catch (failure) {
      reorderError = errorMessage(failure);
    } finally {
      reorderBusy = false;
    }
  }
</script>

{#snippet actions()}
  <button type="button" class="btn btn-ghost" onclick={() => app.openImport()} disabled={app.importOpen}>
    导入脚本
  </button>
  <button
    type="button"
    class="btn btn-primary"
    onclick={() => app.openNewTask()}
    disabled={app.snapshot === null || app.editor.open}
  >
    新建脚本
  </button>
  <Menu label="批量与排序" items={bulkMenu} disabled={app.tasks.length === 0} />
{/snippet}

<div class="page">
  <PageHeader title="控制台" {lead} {actions} />
  <GlobalBanners />

  {#if app.snapshot === null}
    <div class="skeleton-list" aria-label="正在加载">
      <div class="skeleton"></div>
      <div class="skeleton"></div>
    </div>
  {:else if app.tasks.length === 0}
    <EmptyState
      title="还没有脚本"
      text="导入一份脚本清单，或手动新建一个。Rook 负责按时启动、崩溃重启，并把日志留在本机。"
    >
      {#snippet actions()}
        <button type="button" class="btn btn-primary" onclick={() => app.openImport()}>导入脚本</button>
        <button type="button" class="btn btn-ghost" onclick={() => app.openNewTask()}>新建脚本</button>
      {/snippet}
    </EmptyState>
  {:else}
    <section class="section" aria-label="脚本列表">
      <div class="section-head">
        <h2 class="section-title">脚本</h2>
        <span class="section-meta">{app.tasks.length}</span>
        {#if app.attentionTasks.length > 0 || onlyAttention}
          <button type="button" class="btn-link" onclick={() => (onlyAttention = !onlyAttention)}>
            {onlyAttention ? "显示全部" : "只看待处理（" + app.attentionTasks.length + "）"}
          </button>
        {/if}
        {#if batchBusy}
          <span class="section-meta">正在批量处理…</span>
        {:else if batchReport}
          <span class="section-meta">{batchReport}</span>
        {/if}
      </div>

      {#if visible.length === 0}
        <div class="callout">
          <div class="callout-title">没有需要处理的脚本</div>
          <p>所有脚本都处于正常或已停止状态。</p>
          <div class="callout-actions">
            <button type="button" class="btn btn-sm" onclick={() => (onlyAttention = false)}>显示全部脚本</button>
          </div>
        </div>
      {:else}
        <div class="task-grid">
          {#each visible as task (task.id)}
            <TaskCard {task} {now} />
          {/each}
        </div>
      {/if}
    </section>
  {/if}
</div>

{#if pendingBatch}
  <ConfirmDialog
    title={pendingBatch === "start" ? "启动全部脚本" : "停止全部脚本"}
    message={batchMessage}
    confirmLabel={pendingBatch === "start" ? "全部启动" : "全部停止"}
    tone={pendingBatch === "start" ? "normal" : "danger"}
    busy={batchBusy}
    onconfirm={() => void runPendingBatch()}
    oncancel={() => (pendingBatch = null)}
  />
{/if}

{#if reorderOpen}
  <Overlay
    title="调整脚本顺序"
    subtitle="上移或下移会立即写入配置；侧栏与卡片按新顺序显示。"
    onclose={() => (reorderOpen = false)}
  >
    <ul class="reorder-list">
      {#each app.tasks as task, index (task.id)}
        <li class="reorder-row">
          <span class="reorder-name truncate">{task.name || task.id}</span>
          <span class="reorder-id">{task.id}</span>
          <div class="row">
            <button
              type="button"
              class="btn btn-sm btn-ghost"
              aria-label={"上移 " + (task.name || task.id)}
              disabled={reorderBusy || index === 0}
              onclick={() => void move(index, index - 1)}
            >
              ↑
            </button>
            <button
              type="button"
              class="btn btn-sm btn-ghost"
              aria-label={"下移 " + (task.name || task.id)}
              disabled={reorderBusy || index === app.tasks.length - 1}
              onclick={() => void move(index, index + 1)}
            >
              ↓
            </button>
          </div>
        </li>
      {/each}
    </ul>
    {#if reorderError}<p class="field-error">{reorderError}</p>{/if}
  </Overlay>
{/if}
