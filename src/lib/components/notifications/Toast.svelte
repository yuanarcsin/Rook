<script lang="ts">
  // 轻提示：只在通知中心关闭时出现，9 秒自动消失；悬停/聚焦暂停计时。
  import { onMount } from "svelte";
  import { app, type NotificationItem } from "$lib/state/app.svelte";
  import { humanizeNotification } from "$lib/utils/notification-text";
  import Icon from "../ui/Icon.svelte";
  import NotificationIcon from "./NotificationIcon.svelte";

  let { item }: { item: NotificationItem } = $props();

  const LIFETIME_MS = 9000;
  const MIN_REMAINING_MS = 2000;

  let human = $derived(humanizeNotification(item));
  let remaining = LIFETIME_MS;
  let startedAt = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  function schedule() {
    startedAt = Date.now();
    timer = setTimeout(() => {
      timer = undefined;
      app.dismissToast(item.key);
    }, remaining);
  }

  function pause() {
    if (timer === undefined) return;
    clearTimeout(timer);
    timer = undefined;
    remaining = Math.max(MIN_REMAINING_MS, remaining - (Date.now() - startedAt));
  }

  function resume() {
    if (timer === undefined) schedule();
  }

  onMount(() => {
    schedule();
    return () => {
      if (timer !== undefined) clearTimeout(timer);
    };
  });
</script>

<div
  class="toast"
  role="status"
  onmouseenter={pause}
  onmouseleave={resume}
  onfocusin={pause}
  onfocusout={resume}
>
  <NotificationIcon tone={human.tone} kind={item.kind} />
  <div class="toast-text">
    <strong>{human.title}</strong>
    <p>{human.summary}</p>
  </div>
  <div class="toast-actions">
    <button
      type="button"
      class="btn-link"
      onclick={() => {
        app.openNotifications();
        app.dismissToast(item.key);
      }}
    >
      查看
    </button>
    <button
      type="button"
      class="toast-close"
      aria-label="关闭提醒"
      onclick={() => app.dismissToast(item.key)}
    >
      <Icon name="close" size={14} />
    </button>
  </div>
</div>
