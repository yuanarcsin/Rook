<script lang="ts">
  // 控制台脚本卡：标题 + 状态胶囊 + 关键状态 + 三个直接动作（启动/停止、重启、控制台）。
  // 卡片的层级只靠阴影与留白，不描边；动作固定在卡底，跨卡片对齐。
  import type { TaskSnapshot } from "$lib/api";
  import { app } from "$lib/state/app.svelte";
  import { openPanel, runTaskAction } from "$lib/services/tasks";
  import { formatElapsed, formatCountdown, formatWhen } from "$lib/utils/format";
  import { describeSchedule } from "$lib/utils/schedule";
  import {
    canRun,
    displayStatus,
    displayStatusLabel,
    errorMessage,
    pausedReasonLabel,
    statusTone,
    taskActionBusyLabel,
    taskActionLabel,
    type TaskAction,
  } from "$lib/utils/task-utils";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";
  import Icon from "../ui/Icon.svelte";
  import StatusPill from "../ui/StatusPill.svelte";

  let { task, now }: { task: TaskSnapshot; now: number } = $props();

  let pending = $state<"stop" | "restart" | null>(null);
  let panelBusy = $state(false);

  let busy = $derived(app.taskBusy[task.id] ?? null);
  let status = $derived(displayStatus(task));
  let tone = $derived(statusTone(status));
  let runtime = $derived(task.runtime);
  let supervisor = $derived(task.supervisor);
  let schedule = $derived(task.schedule);

  let primary = $derived<TaskAction>(
    supervisor.needsHuman.active && canRun(task, "restart")
      ? "restart"
      : canRun(task, "stop")
        ? "stop"
        : "start",
  );
  let primaryLabel = $derived(
    busy === primary ? taskActionBusyLabel[primary] : taskActionLabel[primary],
  );

  /** 卡片主结论：一条最需要知道的状态补充。 */
  let detail = $derived.by(() => {
    if (supervisor.needsHuman.active) {
      return supervisor.needsHuman.message ?? "等待人工处理";
    }
    if (status === "running" && runtime?.startedAt != null) {
      return "已运行 " + formatElapsed(runtime.startedAt, now);
    }
    if (status === "backoff" && supervisor.nextRetryAt != null) {
      return formatCountdown(supervisor.nextRetryAt, now) + " 后重试";
    }
    if (supervisor.pausedReason) {
      return pausedReasonLabel(supervisor.pausedReason);
    }
    if (!task.valid && task.issues.length > 0) {
      return task.issues[0].message;
    }
    if (status === "stopped" && runtime?.exitCode != null) {
      if (runtime.exitReason === "user_stop") {
        return "上次手动停止";
      }
      return runtime.exitCode === 0 ? "上次运行成功" : "上次运行失败";
    }
    if (status === "disabled") {
      return "启用后才会被监督";
    }
    return "尚未启动";
  });

  /** 第二行：运行时段 / 提权标记，有才显示。 */
  let context = $derived.by(() => {
    const parts: string[] = [];
    if (schedule.enabled) {
      parts.push(describeSchedule(schedule.windows));
      if (schedule.inWindow && schedule.nextEndAt !== null) {
        parts.push(formatWhen(schedule.nextEndAt, now) + " 结束");
      } else if (!schedule.inWindow && schedule.nextStartAt !== null) {
        parts.push("下次 " + formatWhen(schedule.nextStartAt, now));
      }
    }
    if (task.elevated) {
      parts.push("提权未实现");
    }
    return parts;
  });

  /** 文本化上下文：换行时不会在行首留下分隔点。 */
  let contextText = $derived(context.filter((line) => line !== "提权未实现").join(" · "));

  let confirmNeeded = $derived(runtime?.status === "running" && !supervisor.needsHuman.active);

  function request(action: "stop" | "restart") {
    if (confirmNeeded) {
      pending = action;
      return;
    }
    void runTaskAction(task, action);
  }

  function runPrimary() {
    if (primary === "start") {
      void runTaskAction(task, "start");
    } else {
      request(primary);
    }
  }

  function confirmPending() {
    const action = pending;
    pending = null;
    if (action !== null) {
      void runTaskAction(task, action);
    }
  }

  async function openConsole() {
    if (!task.panelUrl || panelBusy) return;
    panelBusy = true;
    try {
      await openPanel(task.id);
    } catch (failure) {
      app.taskErrors = { ...app.taskErrors, [task.id]: errorMessage(failure) };
    } finally {
      panelBusy = false;
    }
  }

  let confirmTitle = $derived(pending === "restart" ? "重启脚本" : "停止脚本");
  let confirmMessage = $derived.by(() => {
    const name = task.name || task.id;
    const grace = Math.max(1, Math.round(task.stop.graceMs / 1000));
    const how =
      task.stop.method === "graceful"
        ? "先请求脚本自行退出（最多 " + grace + " 秒），未退出则强制结束"
        : "立即终止进程树";
    const base =
      pending === "restart"
        ? "重启「" + name + "」会先停止当前进程再启动，期间短暂离线。"
        : "停止「" + name + "」会" + how + "；未保存的状态可能丢失。";
    if (!schedule.enabled) return base;
    return (
      base +
      (schedule.inWindow
        ? "本窗口内不再自动启动；窗口结束会停止该实例。"
        : "下一窗口仍会按计划自动启动。")
    );
  });
</script>

<article class="task-card">
  <header class="task-card-head">
    <div class="task-card-id">
      <span class="task-card-glyph" aria-hidden="true"><Icon name="terminal" size={16} /></span>
      <button
        type="button"
        class="task-card-name"
        title="打开脚本页"
        onclick={() => app.openTask(task.id)}
      >
        {task.name || task.id}
      </button>
    </div>
    <StatusPill {status} {tone} />
  </header>

  <div class="task-card-body">
    <p class="task-card-detail" class:tone-bad={tone === "bad"} class:tone-warn={tone === "warn"}>
      {detail}
    </p>
    {#if contextText.length > 0 || task.elevated}
      <p class="task-card-context">
        {contextText}
        {#if task.elevated}<span class="row-note">提权未实现</span>{/if}
      </p>
    {/if}
    {#if task.plugin?.updateAvailable}
      <p class="task-card-note">清单有更新，可在脚本页里同步</p>
    {/if}
  </div>

  <div class="task-card-foot">
    <div class="task-card-actions">
      <button
        type="button"
        class="btn"
        class:btn-primary={primary !== "stop"}
        class:btn-danger={primary === "stop"}
        onclick={runPrimary}
        disabled={busy !== null || !canRun(task, primary)}
      >
        <Icon name={primary === "stop" ? "stop" : "play"} size={14} />
        {primaryLabel}
      </button>
      {#if primary !== "restart"}
        <button
          type="button"
          class="btn btn-ghost"
          onclick={() => request("restart")}
          disabled={busy !== null || !canRun(task, "restart")}
        >
          <Icon name="refresh" size={14} />
          {busy === "restart" ? "重启中…" : "重启"}
        </button>
      {:else if canRun(task, "start")}
        <button
          type="button"
          class="btn btn-ghost"
          onclick={() => void runTaskAction(task, "start")}
          disabled={busy !== null}
        >
          <Icon name="play" size={14} />
          启动
        </button>
      {/if}
      <button
        type="button"
        class="btn btn-ghost"
        title={task.panelUrl ? "用系统浏览器打开脚本控制台" : "这个脚本没有配置控制台地址"}
        onclick={() => void openConsole()}
        disabled={!task.panelUrl || panelBusy}
      >
        <Icon name="external" size={14} />
        {panelBusy ? "打开中…" : "控制台"}
      </button>
    </div>
    <button
      type="button"
      class="icon-btn card-open"
      aria-label={"打开 " + (task.name || task.id) + " 脚本页"}
      title="脚本详情、编辑与运行时段"
      onclick={() => app.openTask(task.id)}
    >
      <Icon name="chevron-right" size={16} />
    </button>
  </div>

  {#if app.taskErrors[task.id]}
    <p class="task-card-error" role="alert">{app.taskErrors[task.id]}</p>
  {/if}
</article>

{#if pending}
  <ConfirmDialog
    title={confirmTitle}
    message={confirmMessage}
    confirmLabel={pending === "restart" ? "重启" : "停止"}
    tone={pending === "restart" ? "normal" : "danger"}
    onconfirm={confirmPending}
    oncancel={() => (pending = null)}
  />
{/if}
