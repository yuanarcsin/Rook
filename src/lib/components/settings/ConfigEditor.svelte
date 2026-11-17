<script lang="ts">
  // TOML 编辑器：整份文本，保存前校验 revision 与语法；语法错误不写盘。
  import { onMount } from "svelte";
  import { readConfigText, saveConfigText, type ConfigTextError } from "$lib/api";
  import { app } from "$lib/state/app.svelte";
  import { errorMessage } from "$lib/utils/task-utils";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";
  import Overlay from "../ui/Overlay.svelte";

  let loaded = $state(false);
  let loading = $state(true);
  let saving = $state(false);
  let path = $state("");
  let revision = $state("");
  let text = $state("");
  let original = $state("");
  let error = $state<string | null>(null);
  /** 冲突时磁盘上的最新 revision；非 null 表示需要重新加载。 */
  let conflict = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let pending: "reload" | "close" | null = $state(null);

  let dirty = $derived(text !== original);

  onMount(() => {
    void read();
  });

  async function read() {
    loading = true;
    error = null;
    conflict = null;
    notice = null;
    try {
      const view = await readConfigText();
      path = view.path;
      revision = view.revision;
      text = view.text;
      original = view.text;
      loaded = true;
    } catch (failure) {
      error = errorMessage(failure);
      loaded = true;
    } finally {
      loading = false;
    }
  }

  /** 有未保存修改时先确认，避免静默丢弃整份草稿。 */
  function requestReload() {
    if (saving || loading) return;
    if (dirty) pending = "reload";
    else void read();
  }

  function requestClose() {
    if (saving) return;
    if (dirty) pending = "close";
    else app.closeConfig();
  }

  function confirmPending() {
    const action = pending;
    pending = null;
    if (action === "reload") {
      void read();
    } else if (action === "close") {
      app.closeConfig();
    }
  }

  async function save() {
    if (saving || loading) return;
    saving = true;
    error = null;
    conflict = null;
    notice = null;
    try {
      const result = await saveConfigText(text, revision);
      revision = result.revision;
      original = text;
      app.applySnapshot(result.snapshot);
      notice = result.changed ? "已保存并重新加载配置。" : "内容未变化，无需保存。";
    } catch (failure) {
      const detail = failure as ConfigTextError | null;
      if (detail && detail.stage === "conflict") {
        conflict = detail.currentRevision;
        error = "文件已被外部修改：当前草稿基于旧版本，未写入。请重新读取后再编辑。";
      } else if (detail && detail.stage === "parse") {
        const where =
          detail.line === null
            ? ""
            : "第 " + detail.line + " 行" + (detail.col === null ? "" : ":" + detail.col) + "：";
        error = "配置无效：" + where + detail.message;
      } else {
        error = errorMessage(failure);
      }
    } finally {
      saving = false;
    }
  }
</script>

{#snippet footer()}
  <button type="button" class="btn btn-ghost" onclick={requestClose} disabled={saving}>关闭</button>
  <button type="button" class="btn btn-ghost" onclick={requestReload} disabled={saving}>重新读取</button>
  <button
    type="button"
    class="btn btn-primary"
    onclick={() => void save()}
    disabled={saving || loading || !loaded || revision.length === 0}
  >
    {saving ? "保存中…" : "保存配置"}
  </button>
{/snippet}

<Overlay
  title="配置文件"
  subtitle="整份 TOML；保存前校验 revision 与语法，语法错误不会写盘。"
  wide
  onclose={requestClose}
  footer={footer}
>
  {#if loading}
    <p class="detail-note">读取配置中…</p>
  {:else}
    <p class="log-path">
      <code>{path || "—"}</code>
      {#if revision}<span> · revision {revision.slice(0, 12)}</span>{/if}
      {#if dirty}<span class="tag tag-warn">有未保存改动</span>{/if}
    </p>
    <textarea
      class="editor-textarea"
      rows="20"
      spellcheck="false"
      bind:value={text}
      disabled={saving}
      aria-label="配置文件 TOML 文本"
    ></textarea>
    {#if error}<p class="field-error" role="alert">{error}</p>{/if}
    {#if conflict}
      <p class="detail-note">
        磁盘上的最新 revision 是 <code>{conflict.slice(0, 12)}</code>；点「重新读取」会放弃当前草稿。
      </p>
    {/if}
    {#if notice}<p class="detail-note">{notice}</p>{/if}
  {/if}
</Overlay>

{#if pending}
  <ConfirmDialog
    title="放弃未保存的修改？"
    message={pending === "reload"
      ? "重新读取会丢弃当前编辑器里尚未保存的修改。"
      : "关闭后，当前编辑器里尚未保存的修改会丢失。"}
    confirmLabel={pending === "reload" ? "放弃并读取" : "放弃修改"}
    cancelLabel="继续编辑"
    tone="danger"
    onconfirm={confirmPending}
    oncancel={() => (pending = null)}
  />
{/if}
