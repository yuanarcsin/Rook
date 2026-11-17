<script lang="ts">
  // 统一覆盖层：焦点锁定、Esc、滚动锁与嵌套规则。
  import { onMount, type Snippet } from "svelte";
  import {
    focusTopModal,
    hasModal,
    isTopModal,
    popModal,
    pushModal,
    topModalContains,
  } from "$lib/utils/modal-stack";
  import Icon from "./Icon.svelte";

  let {
    title,
    subtitle = null,
    onclose,
    children,
    footer = null,
    wide = false,
    closeOnBackdrop = true,
  }: {
    title: string;
    subtitle?: string | null;
    onclose: () => void;
    children: Snippet;
    footer?: Snippet | null;
    wide?: boolean;
    closeOnBackdrop?: boolean;
  } = $props();

  const FOCUSABLE =
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

  let card = $state<HTMLDivElement | null>(null);
  let previousFocus: HTMLElement | null = null;
  const modalId = Symbol("overlay");
  const titleId = "overlay-title-" + Math.random().toString(36).slice(2, 8);

  onMount(() => {
    previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    pushModal(modalId, card);
    card?.focus();
    return () => {
      popModal(modalId);
      if (!hasModal()) {
        if (previousFocus?.isConnected) previousFocus.focus();
      } else if (topModalContains(previousFocus)) {
        previousFocus?.focus();
      } else {
        focusTopModal();
      }
    };
  });

  function onBackdrop(event: MouseEvent) {
    if (closeOnBackdrop && event.target === event.currentTarget) onclose();
  }

  function onKeydown(event: KeyboardEvent) {
    if (!isTopModal(modalId)) return;
    if (event.key === "Escape") {
      event.preventDefault();
      onclose();
      return;
    }
    if (event.key !== "Tab" || card === null) return;
    const nodes = Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
      (node) => node.offsetParent !== null,
    );
    if (nodes.length === 0) {
      event.preventDefault();
      card.focus();
      return;
    }
    const first = nodes[0];
    const last = nodes[nodes.length - 1];
    const active = document.activeElement;
    if (event.shiftKey && (active === first || active === card)) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="overlay-backdrop" role="presentation" onclick={onBackdrop}>
  <div
    class="overlay-panel"
    class:wide
    role="dialog"
    aria-modal="true"
    aria-labelledby={titleId}
    tabindex="-1"
    bind:this={card}
  >
    <header class="overlay-head">
      <div class="overlay-head-text">
        <h2 class="overlay-title" id={titleId}>{title}</h2>
        {#if subtitle}<p class="overlay-sub">{subtitle}</p>{/if}
      </div>
      <button
        type="button"
        class="icon-btn"
        aria-label="关闭"
        title="关闭（Esc）"
        onclick={onclose}
      >
        <Icon name="close" size={18} />
      </button>
    </header>
    <div class="overlay-body">
      {@render children()}
    </div>
    {#if footer}
      <div class="overlay-foot">{@render footer()}</div>
    {/if}
  </div>
</div>
