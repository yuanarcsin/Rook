<script lang="ts">
  // 通知语气图标：颜色只是第二通道，形状与文字先表达“需要处理 / 等待人工 / 提示”。
  import type { NotificationKind } from "$lib/api";
  import type { NotificationTone } from "$lib/utils/notification-text";
  import Icon from "../ui/Icon.svelte";

  let {
    tone,
    kind,
    size = 16,
  }: { tone: NotificationTone; kind: NotificationKind; size?: number } = $props();
</script>

<span class="notif-icon {tone}" aria-hidden="true">
  {#if tone === "bad"}
    <Icon name="warning" size={size} />
  {:else if kind === "needs_human"}
    <Icon name="user" size={size} />
  {:else if kind === "config_warning"}
    <Icon name="info" size={size} />
  {:else}
    <Icon name="clock" size={size} />
  {/if}
</span>
