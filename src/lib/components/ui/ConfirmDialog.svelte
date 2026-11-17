<script lang="ts">
  // 统一二次确认：危险操作只有这里一种形态，按钮写动词不写“确定”。
  import Overlay from "./Overlay.svelte";

  let {
    title,
    message,
    confirmLabel = "确认",
    cancelLabel = "取消",
    tone = "normal",
    busy = false,
    onconfirm,
    oncancel,
  }: {
    title: string;
    message: string;
    confirmLabel?: string;
    cancelLabel?: string;
    tone?: "normal" | "danger";
    busy?: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();
</script>

{#snippet footer()}
  <button type="button" class="btn btn-ghost" onclick={oncancel} disabled={busy}>
    {cancelLabel}
  </button>
  <button
    type="button"
    class="btn"
    class:btn-danger={tone === "danger"}
    class:btn-primary={tone === "normal"}
    onclick={onconfirm}
    disabled={busy}
  >
    {busy ? "处理中…" : confirmLabel}
  </button>
{/snippet}

<Overlay {title} onclose={oncancel} closeOnBackdrop={false} footer={footer}>
  <div class="confirm-body">
    <p>{message}</p>
  </div>
</Overlay>
