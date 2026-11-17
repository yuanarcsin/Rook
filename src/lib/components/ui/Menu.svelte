<script module lang="ts">
  export type MenuItem = {
    label: string;
    /** 补充说明，同时作为 title。 */
    hint?: string;
    disabled?: boolean;
    danger?: boolean;
    onselect: () => void;
  };
</script>

<script lang="ts">
  // 轻量操作菜单：无第三方依赖，键盘可达（上下/Home/End/Esc）。
  import { tick } from "svelte";
  import Icon from "./Icon.svelte";

  let {
    label,
    items,
    align = "end",
    disabled = false,
  }: {
    label: string;
    items: MenuItem[];
    align?: "start" | "end";
    disabled?: boolean;
  } = $props();

  let open = $state(false);
  let root = $state<HTMLDivElement | null>(null);
  let menu = $state<HTMLDivElement | null>(null);
  let trigger = $state<HTMLButtonElement | null>(null);
  let activeIndex = $state(-1);

  function enabledButtons(): HTMLButtonElement[] {
    if (menu === null) return [];
    return Array.from(
      menu.querySelectorAll<HTMLButtonElement>('button[role="menuitem"]:not([disabled])'),
    );
  }

  function focusItem(index: number) {
    const nodes = enabledButtons();
    if (nodes.length === 0) return;
    const next = ((index % nodes.length) + nodes.length) % nodes.length;
    activeIndex = next;
    nodes[next]?.focus();
  }

  async function openMenu() {
    if (disabled || open) return;
    open = true;
    activeIndex = -1;
    await tick();
    focusItem(0);
  }

  function closeMenu(restoreFocus = true) {
    if (!open) return;
    open = false;
    activeIndex = -1;
    if (restoreFocus) trigger?.focus();
  }

  function choose(item: MenuItem) {
    if (item.disabled) return;
    closeMenu();
    item.onselect();
  }

  function onTriggerKeydown(event: KeyboardEvent) {
    if (event.key === "Escape" && open) {
      event.preventDefault();
      closeMenu();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      if (open) focusItem(0);
      else void openMenu();
    }
  }

  function onMenuKeydown(event: KeyboardEvent) {
    switch (event.key) {
      case "Escape":
        event.preventDefault();
        closeMenu();
        break;
      case "ArrowDown":
        event.preventDefault();
        focusItem(activeIndex + 1);
        break;
      case "ArrowUp":
        event.preventDefault();
        focusItem(activeIndex - 1);
        break;
      case "Home":
        event.preventDefault();
        focusItem(0);
        break;
      case "End":
        event.preventDefault();
        focusItem(enabledButtons().length - 1);
        break;
      case "Tab":
        closeMenu(false);
        break;
    }
  }

  $effect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (root !== null && !root.contains(event.target as Node)) {
        closeMenu(false);
      }
    };
    window.addEventListener("pointerdown", onPointerDown);
    return () => window.removeEventListener("pointerdown", onPointerDown);
  });
</script>

<div class="menu-anchor" bind:this={root}>
  <button
    type="button"
    class="icon-btn"
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label={label}
    title={label}
    bind:this={trigger}
    {disabled}
    onclick={() => (open ? closeMenu() : void openMenu())}
    onkeydown={onTriggerKeydown}
  >
    <Icon name="more" size={18} />
  </button>
  {#if open}
    <div
      class="menu-popover"
      class:align-start={align === "start"}
      role="menu"
      aria-label={label}
      tabindex="-1"
      bind:this={menu}
      onkeydown={onMenuKeydown}
    >
      {#each items as item (item.label)}
        <button
          type="button"
          role="menuitem"
          tabindex="-1"
          class="menu-item"
          class:danger={item.danger}
          title={item.hint}
          disabled={item.disabled}
          onclick={() => choose(item)}
        >
          <span>{item.label}</span>
          {#if item.hint}<span class="menu-hint">{item.hint}</span>{/if}
        </button>
      {/each}
    </div>
  {/if}
</div>
