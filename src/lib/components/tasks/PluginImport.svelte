<script lang="ts">
  // 导入脚本：拖入目录或填写清单路径 → 只读预览 → 导入/同步。
  // 不执行脚本、不显示清单里的密码；错误按阶段给出可行动说明。
  import { onMount } from "svelte";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import {
    importPlugin,
    inspectPlugin,
    readConfigText,
    syncPlugin,
    type PluginInspectView,
    type UnlistenFn,
  } from "$lib/api";
  import { app } from "$lib/state/app.svelte";
  import { describeSchedule } from "$lib/utils/schedule";
  import { errorMessage } from "$lib/utils/task-utils";
  import Overlay from "../ui/Overlay.svelte";

  const MANIFEST_NAME = "rook.plugin.toml";

  let manifestPath = $state("");
  let preview = $state<PluginInspectView | null>(null);
  let inspecting = $state(false);
  let importing = $state(false);
  let syncing = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let dropActive = $state(false);
  let dragSupported = $state(true);
  let dropHint = $state<string | null>(null);

  onMount(() => {
    let unlisten: UnlistenFn | undefined;
    let disposed = false;

    void (async () => {
      try {
        const cleanup = await getCurrentWebview().onDragDropEvent((event) => {
          const payload = event.payload;
          if (payload.type === "enter" || payload.type === "over") {
            dropActive = true;
          } else if (payload.type === "leave") {
            dropActive = false;
          } else if (payload.type === "drop") {
            dropActive = false;
            const first = payload.paths[0];
            if (!first) {
              dropHint = "拖放没有提供路径，请手动输入。";
              return;
            }
            manifestPath = pathFromDrop(first);
            preview = null;
            dropHint = "已填入路径，点「预览」继续。";
          }
        });
        if (disposed) {
          cleanup();
        } else {
          unlisten = cleanup;
        }
      } catch {
        // 浏览器/无 Tauri 环境没有原生拖放：静默降级为手动输入。
        dragSupported = false;
        dropHint = null;
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  /** 拖入文件则直接用；拖入目录则拼上清单文件名。 */
  function pathFromDrop(droppedPath: string): string {
    const normalized = droppedPath.replace(/[\\/]+$/, "");
    if (normalized.toLowerCase().endsWith(".toml")) {
      return normalized;
    }
    return normalized + "/" + MANIFEST_NAME;
  }

  function pluginError(error: unknown): { stage?: string; message?: string } | null {
    if (typeof error === "object" && error !== null && "stage" in error) {
      return error as { stage?: string; message?: string };
    }
    return null;
  }

  function handleFailure(failure: unknown) {
    const detail = pluginError(failure);
    if (detail && detail.stage === "conflict") {
      error = "配置已被外部修改，未写入。请刷新后重新预览。";
      return;
    }
    error = errorMessage(failure);
  }

  async function inspect() {
    if (inspecting) return;
    const target = manifestPath.trim();
    if (target.length === 0) {
      error = "请先输入脚本目录或清单路径。";
      return;
    }
    inspecting = true;
    error = null;
    notice = null;
    preview = null;
    try {
      preview = await inspectPlugin(target);
    } catch (failure) {
      handleFailure(failure);
    } finally {
      inspecting = false;
    }
  }

  async function importNow() {
    if (preview === null || importing) return;
    if (preview.existingTaskId !== null) {
      error = "已存在同 id 脚本，请用「同步现有脚本」或先改名。";
      return;
    }
    importing = true;
    error = null;
    notice = null;
    try {
      const result = await importPlugin(preview.manifestPath, preview.configRevision);
      app.applySnapshot(result.snapshot);
      const imported = result.snapshot.tasks.find((task) => task.id === result.taskId);
      notice =
        "已导入 " +
        result.taskId +
        (imported?.autoStart
          ? "；本次不会自动启动，下次应用启动或手动启动后生效。"
          : "；默认不会自动启动。");
      manifestPath = "";
      preview = null;
    } catch (failure) {
      handleFailure(failure);
    } finally {
      importing = false;
    }
  }

  async function syncExisting() {
    if (preview === null || preview.existingTaskId === null || syncing) return;
    syncing = true;
    error = null;
    notice = null;
    try {
      const config = await readConfigText();
      const result = await syncPlugin(preview.existingTaskId, config.revision);
      app.applySnapshot(result.snapshot);
      notice = result.changed ? "已同步到现有脚本。" : "清单与现有配置一致。";
      preview = null;
      manifestPath = "";
    } catch (failure) {
      handleFailure(failure);
    } finally {
      syncing = false;
    }
  }

  function close() {
    if (importing || syncing) return;
    app.closeImport();
  }
</script>

{#snippet footer()}
  <button type="button" class="btn btn-ghost" onclick={close} disabled={importing || syncing}>
    关闭
  </button>
{/snippet}

<Overlay
  title="导入脚本"
  subtitle="读取 rook.plugin.toml（旧名 campussentry.plugin.toml 仍可）生成脚本；不会执行脚本，也不会显示清单里的密码。"
  wide
  onclose={close}
  footer={footer}
>
  <div class="stack">
    <div class="drop-zone" class:active={dropActive}>
      {#if dragSupported}
        把脚本文件夹或清单文件拖到这里
      {:else}
        原生拖放不可用，请手动输入路径
      {/if}
    </div>
    {#if dropHint}<p class="detail-note">{dropHint}</p>{/if}

    <div class="import-row">
      <label class="field grow">
        <span>清单或脚本目录</span>
        <input
          type="text"
          bind:value={manifestPath}
          placeholder="D:/scripts/ykt 或 …/rook.plugin.toml"
          onkeydown={(event) => {
            if (event.key === "Enter") void inspect();
          }}
        />
      </label>
      <button type="button" class="btn btn-primary" onclick={() => void inspect()} disabled={inspecting}>
        {inspecting ? "读取中…" : "预览"}
      </button>
    </div>

    {#if error}<p class="field-error" role="alert">{error}</p>{/if}
    {#if notice}<div class="callout callout-info"><div class="callout-title">{notice}</div></div>{/if}

    {#if preview}
      <div class="preview-panel">
        <div class="detail-section-head">
          <div>
            <div class="preview-title">{preview.name || preview.pluginId}</div>
            <div class="detail-note">
              <code>{preview.pluginId}</code> · {preview.version || "—"}
              {#if preview.description} · {preview.description}{/if}
            </div>
          </div>
          {#if preview.existingTaskId !== null}
            <span class="tag tag-warn">已存在同 id 脚本</span>
          {:else}
            <span class="tag tag-ok">可导入</span>
          {/if}
        </div>

        <dl class="preview-grid">
          <div>
            <dt>清单路径</dt>
            <dd><code class="path-text">{preview.manifestPath}</code></dd>
          </div>
          <div>
            <dt>命令</dt>
            <dd><code class="path-text">{preview.command}{preview.args.length ? " " + preview.args.join(" ") : ""}</code></dd>
          </div>
          <div>
            <dt>健康检查</dt>
            <dd>
              {preview.healthType}
              {#if preview.healthUrl} · {preview.healthUrl}{/if}
            </dd>
          </div>
          <div>
            <dt>端口</dt>
            <dd>{preview.ports.length ? preview.ports.join(", ") : "—"}</dd>
          </div>
          <div>
            <dt>控制台</dt>
            <dd>{preview.panelUrl ?? "—"}</dd>
          </div>
          <div>
            <dt>运行时段</dt>
            <dd>
              {#if preview.schedule && preview.schedule.enabled}
                {describeSchedule(preview.schedule.windows)}
              {:else}
                未启用
              {/if}
            </dd>
          </div>
        </dl>

        {#if preview.warnings.length > 0}
          <ul class="warning-list">
            {#each preview.warnings as warning, index (index)}
              <li>{warning}</li>
            {/each}
          </ul>
        {/if}

        <details class="details-block">
          <summary>
            更多字段
            <span class="chevron"><svg width="14" height="14" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="m5.5 8 4.5 4.5L14.5 8" /></svg></span>
          </summary>
          <div class="details-body">
            <dl class="kv">
              <dt>脚本目录</dt>
              <dd><code class="path-text">{preview.rootDir}</code></dd>
              <dt>工作目录</dt>
              <dd><code class="path-text">{preview.workingDir}</code></dd>
              <dt>环境变量</dt>
              <dd>{Object.keys(preview.env).length} 项（不显示值）</dd>
              <dt>脚本 id</dt>
              <dd><code>{preview.taskId}</code></dd>
            </dl>
            <p class="field-hint">提权未实现：清单里的 elevated 不会生效，脚本仍以普通权限运行。</p>
          </div>
        </details>

        {#if preview.existingTaskId !== null}
          <div class="callout callout-warn">
            <div class="callout-title">已存在 id 为 {preview.existingTaskId} 的脚本</div>
            <p>导入会被拒绝；可以用「同步现有脚本」只更新清单拥有的字段。</p>
            <div class="callout-actions">
              <button type="button" class="btn btn-sm" onclick={() => void syncExisting()} disabled={syncing}>
                {syncing ? "同步中…" : "同步现有脚本"}
              </button>
              <button type="button" class="btn btn-sm btn-ghost" onclick={() => (preview = null)}>
                取消预览
              </button>
            </div>
          </div>
        {:else}
          <div class="callout-actions">
            <button type="button" class="btn btn-primary" onclick={() => void importNow()} disabled={importing}>
              {importing ? "导入中…" : "导入为脚本"}
            </button>
            <button type="button" class="btn btn-ghost" onclick={() => (preview = null)}>
              取消预览
            </button>
          </div>
        {/if}
      </div>
    {/if}
  </div>
</Overlay>
