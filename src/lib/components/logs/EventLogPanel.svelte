<script lang="ts">
  // 应用事件日志：JSONL 结构化行，只看状态迁移或按任务过滤；点击展开原始行。
  import { onMount } from "svelte";
  import { app } from "$lib/state/app.svelte";
  import { loadEventLog } from "$lib/services/logs";
  import { filterEventLines, parseEventLine } from "$lib/utils/event-filter";
  import { formatBytes, copyText, countMatches, errorMessage, highlightText } from "$lib/utils/task-utils";

  const TAIL_LINES = 200;

  type EventRow = {
    raw: string;
    ok: boolean;
    time: string;
    event: string;
    message: string;
    extra: string;
  };

  let view = $state<Awaited<ReturnType<typeof loadEventLog>> | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let search = $state("");
  let selectedTask = $state<string | null>(null);
  let transitionsOnly = $state(false);
  let expanded = $state<number | null>(null);
  let copied = $state<number | null>(null);

  let tasks = $derived(app.tasks);
  let lines = $derived(view?.lines ?? []);
  let trimmed = $derived(search.trim());
  let filtered = $derived(
    filterEventLines(lines, { taskId: selectedTask, transitionsOnly, search }),
  );
  let rows = $derived(filtered.map(toRow));
  let matches = $derived(countMatches(filtered, search));

  $effect(() => {
    if (selectedTask !== null && !app.tasks.some((task) => task.id === selectedTask)) {
      selectedTask = null;
    }
  });

  onMount(() => {
    void load();
  });

  function toRow(raw: string): EventRow {
    if (raw.trim().length === 0) {
      return { raw, ok: true, time: "", event: "", message: "", extra: "" };
    }
    const record = parseEventLine(raw);
    if (record === null) {
      return { raw, ok: false, time: "", event: "", message: "", extra: "" };
    }
    return {
      raw,
      ok: true,
      time: record.time,
      event: record.event,
      message: record.message,
      extra: record.extra,
    };
  }

  function toggle(index: number) {
    expanded = expanded === index ? null : index;
  }

  async function copyLine(index: number, raw: string) {
    copied = (await copyText(raw)) ? index : null;
  }

  async function load() {
    loading = true;
    error = null;
    try {
      view = await loadEventLog(TAIL_LINES);
    } catch (failure) {
      view = null;
      error = errorMessage(failure);
    } finally {
      loading = false;
    }
  }
</script>

<div class="log-pane">
  <div class="log-toolbar">
    <label class="field grow">
      <span>搜索</span>
      <input type="search" bind:value={search} placeholder="过滤事件（按原始行匹配）" />
    </label>
    <label class="field">
      <span>脚本</span>
      <select bind:value={selectedTask}>
        <option value={null}>全部脚本</option>
        {#each tasks as task (task.id)}
          <option value={task.id}>{task.name || task.id}</option>
        {/each}
      </select>
    </label>
    <label class="toggle">
      <input type="checkbox" bind:checked={transitionsOnly} />
      <span class="toggle-track"></span>
      只看状态迁移
    </label>
    <button type="button" class="btn btn-sm btn-ghost" onclick={() => void load()} disabled={loading}>
      {loading ? "读取中…" : "刷新"}
    </button>
  </div>

  {#if view}
    <p class="log-meta">
      <span class="truncate" title={view.path}>{view.path}</span>
      <span>
        {formatBytes(view.totalBytes)} · 显示 {filtered.length} / 共 {lines.length} 行
        {#if trimmed.length > 0} · 命中 {matches} 处{/if}
      </span>
    </p>
  {/if}

  {#if error}
    <p class="log-error" role="alert">{error}</p>
  {:else if view === null}
    <p class="log-empty">读取中…</p>
  {:else if filtered.length === 0}
    <p class="log-empty">
      {trimmed.length > 0 || selectedTask !== null || transitionsOnly
        ? "没有匹配的行。"
        : "事件日志为空。"}
    </p>
  {:else}
    <div class="event-list" aria-label="应用事件日志（只读）">
      {#each rows as row, index (index)}
        <button
          type="button"
          class="event-row"
          aria-expanded={expanded === index}
          title={row.ok ? "点击展开原始 JSON" : "无法解析，点击查看原文"}
          onclick={() => toggle(index)}
        >
          {#if row.ok}
            <span class="event-time">{row.time}</span>
            <span class="event-name">{row.event}</span>
            <span class="event-msg">
              {#each highlightText(row.message, search) as segment, part (part)}{#if segment.hit}<mark>{segment.text}</mark>{:else}{segment.text}{/if}{/each}
            </span>
            {#if row.extra}<span class="event-fields">{row.extra}</span>{/if}
          {:else}
            <span class="event-msg">
              {#each highlightText(row.raw, search) as segment, part (part)}{#if segment.hit}<mark>{segment.text}</mark>{:else}{segment.text}{/if}{/each}
            </span>
          {/if}
        </button>
        {#if expanded === index}
          <div class="event-raw">
            <pre>{row.raw}</pre>
            <div class="row">
              <button type="button" class="btn btn-sm btn-ghost" onclick={() => void copyLine(index, row.raw)}>
                复制原始行
              </button>
              {#if copied === index}<span class="detail-note">已复制。</span>{/if}
            </div>
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
