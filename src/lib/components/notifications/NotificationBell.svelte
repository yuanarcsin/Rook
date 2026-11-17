<script lang="ts">
  // 通知中心：侧栏底部的「通知」行 + 向右展开的面板。列表说“谁、出了什么事”，技术细节逐条展开。
  import { tick } from "svelte";
  import { app } from "$lib/state/app.svelte";
  import { formatTimestamp } from "$lib/utils/format";
  import {
    formatRelativeTime,
    humanizeNotification,
    notificationKindLabel,
  } from "$lib/utils/notification-text";
  import Icon from "../ui/Icon.svelte";
  import NotificationIcon from "./NotificationIcon.svelte";

  let root = $state<HTMLDivElement | null>(null);
  let panel = $state<HTMLDivElement | null>(null);
  let bell = $state<HTMLButtonElement | null>(null);
  let expanded = $state<number | null>(null);
  let onlyUnread = $state(false);

  let unread = $derived(app.unreadCount);
  let total = $derived(app.notifications.length);
  // “未读”筛选下，正在展开的那条即使刚被标为已读也留在原位，避免详情被抽走。
  let items = $derived(
    onlyUnread
      ? app.notifications.filter((item) => !item.read || item.key === expanded)
      : app.notifications,
  );

  $effect(() => {
    if (app.notificationsOpen) {
      void tick().then(() => panel?.focus());
    }
  });

  function close() {
    app.closeNotifications();
    bell?.focus();
  }

  function onWindowClick(event: MouseEvent) {
    if (!app.notificationsOpen) return;
    const target = event.target;
    if (target instanceof Node && root?.contains(target)) return;
    app.closeNotifications();
  }

  function onWindowKeydown(event: KeyboardEvent) {
    // 面板有焦点时才处理 Esc：上层可能还开着编辑器或导入对话框。
    if (!app.notificationsOpen || event.key !== "Escape") return;
    const active = document.activeElement;
    if (active instanceof Node && root?.contains(active)) {
      event.preventDefault();
      close();
    }
  }

  function toggleItem(key: number) {
    app.markRead(key);
    expanded = expanded === key ? null : key;
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={onWindowKeydown} />

<div class="notif-anchor notif-side" bind:this={root}>
  <button
    type="button"
    class="nav-item notif-side-item"
    bind:this={bell}
    aria-label={unread > 0 ? "通知，" + unread + " 条未读" : "通知"}
    aria-haspopup="dialog"
    aria-expanded={app.notificationsOpen}
    title="通知"
    onclick={() => app.toggleNotifications()}
  >
    <span class="nav-icon"><Icon name="bell" size={18} /></span>
    <span class="notif-side-label">通知</span>
    {#if unread > 0}
      <span class="notif-side-count">{unread > 9 ? "9+" : unread}</span>
    {/if}
  </button>

  {#if app.notificationsOpen}
    <div class="notif-card" role="dialog" aria-label="通知" tabindex="-1" bind:this={panel}>
      <header class="notif-head">
        <h2>通知</h2>
        <span class="notif-count">{unread > 0 ? unread + " 条未读" : "都看过了"}</span>
        <div class="notif-head-actions">
          <button type="button" class="btn-link" onclick={() => app.markAllRead()} disabled={unread === 0}>
            全部已读
          </button>
          <button type="button" class="btn-link" onclick={() => app.clearNotifications()} disabled={total === 0}>
            清空
          </button>
        </div>
      </header>

      {#if total > 0}
        <div class="segmented" role="tablist" aria-label="通知筛选">
          <button type="button" role="tab" aria-selected={!onlyUnread} onclick={() => (onlyUnread = false)}>
            全部 {total}
          </button>
          <button type="button" role="tab" aria-selected={onlyUnread} onclick={() => (onlyUnread = true)}>
            未读 {unread}
          </button>
        </div>
      {/if}

      <div class="notif-list">
        {#if items.length === 0}
          <div class="notif-empty">
            {#if total === 0}
              <strong>暂时没有通知</strong>
              <span>脚本需要你处理时，会在这里留一条记录。</span>
            {:else}
              <strong>没有未读通知</strong>
              <span>切到「全部」可以回看之前的提醒。</span>
            {/if}
          </div>
        {:else}
          {#each items as item (item.key)}
            {@const human = humanizeNotification(item)}
            {@const open = expanded === item.key}
            <div class="notif-item" class:unread={!item.read}>
              <button
                type="button"
                class="notif-row"
                aria-expanded={open}
                onclick={() => toggleItem(item.key)}
              >
                <NotificationIcon tone={human.tone} kind={item.kind} />
                <span class="notif-body">
                  <span class="notif-title">{human.title}</span>
                  <span class="notif-summary">{human.summary}</span>
                  <span class="notif-meta">
                    {formatRelativeTime(item.at)}
                    {#if item.count > 1}
                      · 涉及 {item.count} 个脚本
                    {:else if item.taskName}
                      · {item.taskName}
                    {/if}
                  </span>
                </span>
              </button>

              {#if open}
                {@const taskId = item.taskId}
                <div class="notif-detail">
                  <dl>
                    <dt>时间</dt>
                    <dd>{formatTimestamp(item.at)}</dd>
                    {#if item.taskName}
                      <dt>脚本</dt>
                      <dd>
                        {item.taskName}
                        {#if taskId}<code>{taskId}</code>{/if}
                      </dd>
                    {/if}
                    <dt>类型</dt>
                    <dd>
                      {notificationKindLabel[item.kind]}
                      <code>{item.kind}</code>
                    </dd>
                    <dt>原始信息</dt>
                    <dd class="notif-raw">{item.message}</dd>
                  </dl>
                  <div class="notif-actions">
                    {#if taskId}
                      <button
                        type="button"
                        class="btn btn-ghost btn-sm"
                        onclick={() => {
                          app.openLogsFor(taskId);
                          app.closeNotifications();
                        }}
                      >
                        查看日志
                      </button>
                    {/if}
                    {#if item.kind === "config_error"}
                      <button
                        type="button"
                        class="btn btn-ghost btn-sm"
                        onclick={() => {
                          app.openConfig();
                          app.closeNotifications();
                        }}
                      >
                        打开配置
                      </button>
                    {/if}
                    <button
                      type="button"
                      class="btn btn-ghost btn-sm"
                      onclick={() => {
                        app.dismissNotification(item.key);
                        expanded = null;
                      }}
                    >
                      删除这条
                    </button>
                  </div>
                </div>
              {/if}
            </div>
          {/each}
        {/if}
      </div>

      <footer class="notif-foot">只保留本次打开窗口期间的通知</footer>
    </div>
  {/if}
</div>
