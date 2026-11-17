<script lang="ts">
  import { displayStatusLabel, type StatusTone } from "$lib/utils/task-utils";

  let {
    status,
    tone,
  }: { status: string; tone: StatusTone } = $props();
</script>

<span class="status-pill pill-{tone}">{displayStatusLabel[status] ?? status}</span>
