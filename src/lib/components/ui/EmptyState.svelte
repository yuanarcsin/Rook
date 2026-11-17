<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    title,
    text,
    actions = null,
  }: { title: string; text?: string; actions?: Snippet | null } = $props();
</script>

<div class="empty">
  <div class="empty-mark" aria-hidden="true"></div>
  <h2 class="empty-title">{title}</h2>
  {#if text}<p class="empty-text">{text}</p>{/if}
  {#if actions}
    <div class="empty-actions">{@render actions()}</div>
  {/if}
</div>
