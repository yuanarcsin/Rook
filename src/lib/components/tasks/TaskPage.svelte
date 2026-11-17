<script lang="ts">
  // 脚本页：每个被监督的脚本自己的“工作区”。
  // 布局：面包屑 → 标题/动作 → 提示 → 主列（时段/配置/技术细节）+ 侧列（运行/插件/日志）。
  import type { TaskSnapshot } from "$lib/api";
  import { app } from "$lib/state/app.svelte";
  import { openPanel, runTaskAction, syncTaskFromManifest } from "$lib/services/tasks";
  import {
    canRun,
    displayStatus,
    displayStatusLabel,
    errorMessage,
    formatBytes,
    pausedReasonLabel,
    statusTone,
    taskActionBusyLabel,
    taskActionLabel,
    type TaskAction,
  } from "$lib/utils/task-utils";
  import {
    formatCountdown,
    formatElapsed,
    formatTimestamp,
    shortId,
  } from "$lib/utils/format";
  import GlobalBanners from "../shell/GlobalBanners.svelte";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";
  import Icon from "../ui/Icon.svelte";
  import StatusPill from "../ui/StatusPill.svelte";
  import ScheduleEditor from "./ScheduleEditor.svelte";

  let { task }: { task: TaskSnapshot } = $props();

  let now = $state(Date.now());
  let pending = $state<"stop" | "restart" | null>(null);
  let syncing = $state(false);
  let panelBusy = $state(false);
  let panelError = $state<string | null>(null);
  let syncError = $state<string | null>(null);
  let syncNotice = $state<string | null>(null);

  $effect(() => {
    const timer = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(timer);
  });

  const healthTypeLabel: Record<string, string> = {
    process: "进程存活",
    http: "HTTP 探测",
    heartbeat: "脚本心跳",
  };
  const exitReasonLabel: Record<string, string> = {
    natural: "正常退出",
    user_stop: "用户停止",
  };
  const runtimeStageLabel: Record<string, string> = {
    port_check: "端口检查",
    spawn: "启动进程",
    logs: "日志",
    watch: "进程监控",
    stop: "停止",
    invalid_state: "状态异常",
  };
  const stopMethodLabel: Record<string, string> = {
    graceful: "优雅停止",
    terminate: "立即终止",
  };

  let runtime = $derived(task.runtime);
  let supervisor = $derived(task.supervisor);
  let human = $derived(supervisor.needsHuman);
  let status = $derived(displayStatus(task));
  let tone = $derived(statusTone(status));
  let busy = $derived(app.taskBusy[task.id] ?? null);
  let uptime = $derived(
    runtime?.status === "running" ? formatElapsed(runtime.startedAt, now) : "—",
  );

  let lead = $derived.by(() => {
    if (human.active) {
      return (human.message ?? "等待人工处理") + " · 自动重启已暂停";
    }
    if (supervisor.pausedReason) {
      return pausedReasonLabel(supervisor.pausedReason) + " · 自动重启已关闭";
    }
    if (!task.valid) {
      return "配置有误，暂时无法启动";
    }
    if (status === "backoff" && supervisor.nextRetryAt !== null) {
      return formatCountdown(supervisor.nextRetryAt, now) + " 后重试 · 连续失败 " + supervisor.consecutiveFailures;
    }
    if (status === "running") {
      const pid = runtime?.pid !== null && runtime?.pid !== undefined ? " · PID " + runtime.pid : "";
      return "已运行 " + formatElapsed(runtime?.startedAt, now) + pid;
    }
    if (status === "starting") {
      return "正在启动，等待健康检查…";
    }
    if (status === "stopping") {
      return "正在停止，进程退出后收敛…";
    }
    if (status === "disabled") {
      return "已禁用：启用后才会被监督";
    }
    if (runtime?.exitCode !== null && runtime?.exitCode !== undefined) {
      const reason = runtime.exitReason ? " · " + (exitReasonLabel[runtime.exitReason] ?? runtime.exitReason) : "";
      if (runtime.exitReason === "user_stop") {
        return "上次手动停止" + reason;
      }
      const outcome =
        runtime.exitCode === 0 ? "上次运行成功" : "上次运行失败（退出码 " + runtime.exitCode + "）";
      return outcome + reason;
    }
    return "尚未启动";
  });

  let primary = $derived<TaskAction>(
    human.active && canRun(task, "restart")
      ? "restart"
      : canRun(task, "stop")
        ? "stop"
        : "start",
  );
  let primaryLabel = $derived(
    busy === primary ? taskActionBusyLabel[primary] : taskActionLabel[primary],
  );
  let confirmNeeded = $derived(runtime?.status === "running" && !human.active);

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
    if (!task.schedule.enabled) return base;
    return (
      base +
      (task.schedule.inWindow
        ? "本窗口内不再自动启动；窗口结束会停止该实例。"
        : "下一窗口仍会按计划自动启动。")
    );
  });

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

  async function sync() {
    if (syncing) return;
    syncing = true;
    syncError = null;
    syncNotice = null;
    try {
      syncNotice = await syncTaskFromManifest(task.id);
    } catch (error) {
      const detail = error as { stage?: string } | null;
      syncError =
        detail && detail.stage === "conflict"
          ? "配置已被外部修改，未写入。请刷新后重试。"
          : errorMessage(error);
    } finally {
      syncing = false;
    }
  }

  async function openConsole() {
    if (!task.panelUrl || panelBusy) return;
    panelBusy = true;
    panelError = null;
    try {
      await openPanel(task.id);
    } catch (failure) {
      panelError = errorMessage(failure);
    } finally {
      panelBusy = false;
    }
  }
</script>

<div class="page">
  <nav class="crumbs" aria-label="位置">
    <button type="button" class="btn-link crumb-back" onclick={() => app.setView("console")}>
      <Icon name="arrow-left" size={14} />
      控制台
    </button>
    <span class="crumb-sep">/</span>
    <code class="crumb-id">{task.id}</code>
  </nav>

  <header class="task-hero">
    <div class="task-hero-text">
      <div class="task-title-row">
        <h1 class="page-title">{task.name || task.id}</h1>
        <StatusPill {status} {tone} />
      </div>
      <p class="page-lead">{lead}</p>
    </div>
    <div class="head-actions">
      <button
        type="button"
        class="btn"
        class:btn-primary={primary !== "stop"}
        class:btn-danger={primary === "stop"}
        onclick={runPrimary}
        disabled={busy !== null || !canRun(task, primary)}
      >
        {primaryLabel}
      </button>
      {#if primary !== "restart"}
        <button
          type="button"
          class="btn btn-secondary"
          onclick={() => request("restart")}
          disabled={busy !== null || !canRun(task, "restart")}
        >
          {busy === "restart" ? "重启中…" : "重启"}
        </button>
      {:else if canRun(task, "start")}
        <button
          type="button"
          class="btn btn-secondary"
          onclick={() => void runTaskAction(task, "start")}
          disabled={busy !== null}
        >
          启动
        </button>
      {/if}
      {#if task.panelUrl}
        <button type="button" class="btn btn-ghost" onclick={() => void openConsole()} disabled={panelBusy}>
          {panelBusy ? "打开中…" : "控制台"}
        </button>
      {/if}
      <button
        type="button"
        class="btn btn-ghost"
        onclick={() => app.openEditTask(task.id)}
        disabled={app.editor.open}
      >
        编辑
      </button>
    </div>
  </header>

  <GlobalBanners />

  {#if panelError}
    <p class="field-error" role="alert">打开控制台失败：{panelError}</p>
  {/if}
  {#if app.taskErrors[task.id]}
    <p class="field-error" role="alert">{app.taskErrors[task.id]}</p>
  {/if}

  {#if !task.valid}
    <div class="callout callout-danger">
      <div class="callout-title">配置有误，无法启动</div>
      <p>修正下方的问题后再启动；正在运行的实例不受影响。</p>
    </div>
  {:else if human.active}
    <div class="callout callout-warn">
      <div class="callout-title">等待人工处理{human.message ? "：" + human.message : ""}</div>
      <p>
        自动重启已暂停{canRun(task, "restart") ? "；脚本恢复后会自动继续监督，也可以点「重启」立即重试" : "；脚本恢复后会自动继续监督"}。
        {#if task.schedule.enabled}窗口结束仍会停止该实例。{/if}
      </p>
    </div>
  {:else if supervisor.pausedReason}
    <div class="callout callout-danger">
      <div class="callout-title">已暂停 · {pausedReasonLabel(supervisor.pausedReason)}</div>
      <p>自动重启已关闭。查看日志确认原因后，可以手动启动。</p>
    </div>
  {/if}

  <div class="detail-grid">
    <div class="detail-main">
      <section class="card card-pad">
        <ScheduleEditor taskId={task.id} initial={task.schedule} />
      </section>

      <section class="card card-pad">
        <div class="card-head-row">
          <h2 class="card-title">配置</h2>
          <button
            type="button"
            class="btn-link"
            onclick={() => app.openEditTask(task.id)}
            disabled={app.editor.open}
          >
            编辑脚本
          </button>
        </div>
        <dl class="kv">
          <dt>命令</dt>
          <dd><code class="path-text">{task.command}{task.args.length ? " " + task.args.join(" ") : ""}</code></dd>
          <dt>工作目录</dt>
          <dd><code class="path-text">{task.workingDir}</code></dd>
          <dt>健康检查</dt>
          <dd>
            <span title={task.healthType}>{healthTypeLabel[task.healthType] ?? task.healthType}</span>
            {#if task.ports.length}<span>· 端口 {task.ports.join(", ")}</span>{/if}
          </dd>
          <dt>控制台</dt>
          <dd>{#if task.panelUrl}<code class="path-text">{task.panelUrl}</code>{:else}—{/if}</dd>
        </dl>
        <div class="flags">
          <span class="flag" class:on={task.enabled}>{task.enabled ? "已启用" : "已禁用"}</span>
          <span class="flag" class:on={task.autoStart}>{task.autoStart ? "随应用启动" : "不随应用启动"}</span>
          <span class="flag" class:on={task.notify}>{task.notify ? "会通知" : "不通知"}</span>
        </div>
        {#if task.elevated}
          <div class="callout callout-warn">
            <div class="callout-title">提权未实现</div>
            <p>该配置不会改变实际权限，脚本仍以普通权限运行。</p>
          </div>
        {/if}
        {#if task.issues.length > 0}
          <ul class="issue-list">
            {#each task.issues as issue, index (index)}
              <li class={issue.severity}>
                <code>{issue.path}</code>
                <span>{issue.message}</span>
                {#if issue.line !== null}<span>第 {issue.line} 行</span>{/if}
              </li>
            {/each}
          </ul>
        {/if}
      </section>

      <section class="card card-pad">
        <details class="details-block inline-card">
          <summary>
            技术细节
            <span class="chevron"><Icon name="chevron" size={14} /></span>
          </summary>
          <div class="details-body">
            <dl class="kv">
              <dt>实例 ID</dt>
              <dd>{#if runtime?.instanceId}<code title={runtime.instanceId}>{shortId(runtime.instanceId)}</code>{:else}—{/if}</dd>
              <dt>启动时间</dt>
              <dd>{formatTimestamp(runtime?.startedAt)}</dd>
              <dt>上次退出</dt>
              <dd>
                退出码 {runtime?.exitCode ?? "—"}
                {#if runtime?.exitReason}· {exitReasonLabel[runtime.exitReason] ?? runtime.exitReason}{/if}
              </dd>
              <dt>强制终止</dt>
              <dd>{runtime ? (runtime.forcedKill ? "是" : "否") : "—"}</dd>
              <dt>停止方式</dt>
              <dd>
                {stopMethodLabel[task.stop.method] ?? task.stop.method}
                {#if task.stop.method === "graceful"}· 宽限 {Math.round(task.stop.graceMs / 1000)} 秒{/if}
                · 预算 {Math.round(task.stop.timeoutMs / 1000)} 秒
              </dd>
              <dt>上次状态</dt>
              <dd>{supervisor.lastStatus ? displayStatusLabel[supervisor.lastStatus] ?? supervisor.lastStatus : "—"}</dd>
              <dt>上次错误</dt>
              <dd>
                {#if runtime?.lastError}
                  <code title={runtime.lastError.stage}>{runtimeStageLabel[runtime.lastError.stage] ?? runtime.lastError.stage}</code>
                  {#if runtime.lastError.code !== null}<span>code {runtime.lastError.code}</span>{/if}
                  <span>{runtime.lastError.message}</span>
                {:else}
                  —
                {/if}
              </dd>
              <dt>日志文件</dt>
              <dd>{#if runtime?.logPath}<code class="path-text">{runtime.logPath}</code>{:else}—{/if}</dd>
              <dt>插件清单</dt>
              <dd>{#if task.plugin}<code class="path-text">{task.plugin.manifestPath}</code>{:else}—{/if}</dd>
              <dt>日志统计</dt>
              <dd>
                {#if runtime}
                  写入 {formatBytes(runtime.logStats.writtenBytes)} · 读取 {formatBytes(runtime.logStats.readBytes)}
                  · 丢弃 {formatBytes(runtime.logStats.droppedBytes)} · 轮转 {runtime.logStats.rotations}
                  · 队列丢弃 {runtime.logStats.queueDrops} · IO 错误 {runtime.logStats.ioErrors}
                {:else}
                  —
                {/if}
              </dd>
            </dl>
          </div>
        </details>
      </section>
    </div>

    <aside class="detail-side">
      <section class="card card-pad">
        <div class="card-head-row">
          <h2 class="card-title">运行状态</h2>
          {#if supervisor.restartPending}<span class="tag tag-warn">配置待重启生效</span>{/if}
        </div>
        <dl class="kv">
          <dt>状态</dt>
          <dd><StatusPill {status} {tone} /></dd>
          <dt>已运行</dt>
          <dd>{uptime}</dd>
          <dt>PID</dt>
          <dd>{runtime?.pid ?? "—"}</dd>
          <dt>启动 / 重启</dt>
          <dd>{supervisor.totalStarts} / {supervisor.totalRestarts}</dd>
          <dt>连续失败</dt>
          <dd>{supervisor.consecutiveFailures}</dd>
          <dt>下次重试</dt>
          <dd>
            {#if supervisor.nextRetryAt === null}
              —
            {:else if supervisor.nextRetryAt <= now}
              即将重试
            {:else}
              {formatCountdown(supervisor.nextRetryAt, now)} 后
            {/if}
          </dd>
        </dl>
      </section>

      {#if task.plugin}
        {@const plugin = task.plugin}
        <section class="card card-pad">
          <div class="card-head-row">
            <h2 class="card-title">插件</h2>
            {#if plugin.updateAvailable}<span class="tag tag-warn">清单有更新</span>{/if}
          </div>
          <dl class="kv">
            <dt>id / 版本</dt>
            <dd><code>{plugin.id}</code><span>{plugin.version ?? "—"}</span></dd>
          </dl>
          <div class="card-actions">
            <button
              type="button"
              class="btn btn-sm btn-secondary"
              title="只更新清单拥有的字段，保留启用/自启动/通知等偏好"
              onclick={() => void sync()}
              disabled={syncing}
            >
              {syncing ? "同步中…" : "从清单同步"}
            </button>
          </div>
          {#if syncNotice}<p class="detail-note">{syncNotice}</p>{/if}
          {#if syncError}<p class="field-error" role="alert">{syncError}</p>{/if}
        </section>
      {/if}

      <section class="card card-pad">
        <div class="card-head-row">
          <h2 class="card-title">日志</h2>
          <span class="detail-note">只读</span>
        </div>
        <p class="detail-note">脚本原始输出与轮转归档都在日志页查看。</p>
        <div class="card-actions">
          <button type="button" class="btn btn-sm btn-secondary" onclick={() => app.openLogsFor(task.id)}>
            查看日志
          </button>
        </div>
      </section>
    </aside>
  </div>
</div>

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
