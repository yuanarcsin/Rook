<script lang="ts">
  import type { Snippet } from "svelte";

  let {
    title,
    lead = null,
    actions = null,
  }: { title: string; lead?: string | null; actions?: Snippet | null } = $props();
</script>

<header class="page-head">
  <div class="page-head-text">
    <h1 class="page-title">{title}</h1>
    {#if lead}<p class="page-lead">{lead}</p>{/if}
  </div>
  <div class="head-actions">
    {#if actions}{@render actions()}{/if}
  </div>
</header>
