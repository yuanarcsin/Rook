<script lang="ts">
  // 任务日志面板：工具栏 + 只读文本区。轮询与选择逻辑在 LogViewState。
  import type { LogViewState } from "$lib/state/log-view.svelte";
  import { TAIL_OPTIONS } from "$lib/utils/log-poll";
  import { formatTimestamp } from "$lib/utils/format";
  import { formatBytes, highlightText } from "$lib/utils/task-utils";

  let { log }: { log: LogViewState } = $props();

  let logBody = $state<HTMLElement | null>(null);

  // 跟随模式：每次成功读取后滚到底部（只读预览，不修改文件）。
  $effect(() => {
    if (log.follow && log.view !== null && logBody !== null && log.trimmedSearch.length === 0) {
      logBody.scrollTop = logBody.scrollHeight;
    }
  });

  function scrollToBottom() {
    if (logBody !== null) {
      logBody.scrollTop = logBody.scrollHeight;
    }
  }
</script>

<div class="log-pane">
  <div class="log-toolbar">
    <label class="field">
      <span>文件</span>
      <select value={log.selectedFile} onchange={(event) => log.selectFile(event.currentTarget.value)}>
        <option value="current.log">当前日志</option>
        {#each log.files as file (file.id)}
          {#if !file.current}
            <option value={file.id}>{file.name}（{formatBytes(file.size)}）</option>
          {/if}
        {/each}
      </select>
    </label>
    <label class="field">
      <span>最近</span>
      <select bind:value={log.tailLines}>
        {#each TAIL_OPTIONS as option (option)}
          <option value={option}>{option} 行</option>
        {/each}
      </select>
    </label>
    <label class="field grow">
      <span>搜索</span>
      <input type="search" bind:value={log.search} placeholder={"搜索当前 " + log.tailLines + " 行"} />
    </label>
  </div>

  <div class="log-toolbar-row">
    <label class="toggle" title="每 2 秒读取一次增量">
      <input type="checkbox" bind:checked={log.autoRefresh} />
      <span class="toggle-track"></span>
      自动刷新
    </label>
    <label class="toggle" title="保持滚动到最新输出">
      <input type="checkbox" bind:checked={log.follow} />
      <span class="toggle-track"></span>
      跟随
    </label>
    {#if !log.autoRefresh || log.isArchive}
      <button type="button" class="btn btn-sm btn-ghost" onclick={() => void log.refresh()} disabled={log.loading}>
        {log.loading ? "读取中…" : "刷新"}
      </button>
    {/if}
    {#if !log.follow}
      <button type="button" class="btn btn-sm btn-ghost" onclick={scrollToBottom}>回到底部</button>
    {/if}
    <span class="grow"></span>
    <span class="log-meta">
      {#if log.view}
        <span>{log.view.lines.length} 行 · {formatBytes(log.view.totalBytes)} · 修改于 {formatTimestamp(log.view.modifiedAt)}</span>
      {/if}
      {#if log.trimmedSearch.length > 0}
        <span>命中 {log.matchCount} · 显示 {log.filteredLines.length} 行</span>
      {/if}
    </span>
  </div>

  {#if log.view}
    <p class="log-path" title={log.view.path}><code>{log.view.path}</code></p>
  {/if}

  {#if log.filesError}
    <p class="log-note">日志文件列表读取失败：{log.filesError}</p>
  {/if}
  {#if log.isArchive}
    <p class="log-note">正在查看归档文件（静态），自动刷新已跳过；切回「当前日志」恢复。</p>
  {/if}
  {#if log.view?.truncated}
    <p class="log-note">仅显示最近 {log.tailLines} 行（上限 256KB）。</p>
  {/if}
  {#if log.view?.warning}
    <p class="log-note">{log.view.warning}</p>
  {/if}

  {#if log.error}
    <p class="log-error" role="alert">{log.error}</p>
    {#if log.view === null}
      <p class="log-empty">日志文件不存在或不可读；脚本可能尚未启动。</p>
    {/if}
  {:else if log.view === null}
    <p class="log-empty">读取中…</p>
  {:else if log.filteredLines.length === 0}
    <p class="log-empty">
      {log.trimmedSearch.length > 0 ? "没有匹配的行。" : "暂无日志输出。"}
    </p>
  {:else}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -- 只读滚动区需要键盘可达（axe scrollable-region-focusable） -->
    <pre class="log-view" bind:this={logBody} tabindex="0" aria-label="脚本日志（只读）">{#each log.filteredLines as line, index (index)}<span class="line">{#if line.length === 0}{" "}{:else}{#each highlightText(line, log.search) as segment, part (part)}{#if segment.hit}<mark>{segment.text}</mark>{:else}{segment.text}{/if}{/each}{/if}</span>
{/each}</pre>
  {/if}
</div>
