<script module lang="ts">
  /** 统一的线性图标：20 栅格、1.6 描边、圆头；只表达语义，不承担装饰。 */
  export type IconName =
    | "bell"
    | "console"
    | "logs"
    | "settings"
    | "terminal"
    | "chevron-right"
    | "plus"
    | "more"
    | "close"
    | "chevron"
    | "external"
    | "copy"
    | "refresh"
    | "play"
    | "stop"
    | "search"
    | "warning"
    | "user"
    | "info"
    | "clock"
    | "folder"
    | "file"
    | "plug"
    | "trash"
    | "power"
    | "panel-left"
    | "win-minimize"
    | "win-maximize"
    | "win-restore"
    | "win-close"
    | "arrow-left";
</script>

<script lang="ts">
  let {
    name,
    size = 18,
    stroke = 1.6,
  }: { name: IconName; size?: number; stroke?: number } = $props();
</script>

<svg
  class="icon"
  width={size}
  height={size}
  viewBox="0 0 20 20"
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === "console"}
    <path d="M4.4 14.2a5.8 5.8 0 0 1 11.2 0" />
    <path d="M10 14.2 12.9 10.5" />
    <circle cx="10" cy="14.2" r="1.1" fill="currentColor" stroke="none" />
  {:else if name === "logs"}
    <path d="M5.6 3.6h5.9L15 7v9.4H5.6Z" />
    <path d="M11.5 3.6V7H15" />
    <path d="M7.8 10h4.9M7.8 12.6h4.9" />
  {:else if name === "settings"}
    <path d="M3.8 6.4h12.4M3.8 10h12.4M3.8 13.6h12.4" />
    <circle cx="7.4" cy="6.4" r="1.4" fill="currentColor" stroke="none" />
    <circle cx="12.4" cy="10" r="1.4" fill="currentColor" stroke="none" />
    <circle cx="8.6" cy="13.6" r="1.4" fill="currentColor" stroke="none" />
  {:else if name === "terminal"}
    <rect x="3.4" y="4.2" width="13.2" height="11.6" rx="2.6" />
    <path d="m6.6 8 2 2-2 2M10.6 12h3" />
  {:else if name === "chevron-right"}
    <path d="m8 5.5 4.5 4.5L8 14.5" />
  {:else if name === "bell"}
    <path d="M5.4 8.4a4.6 4.6 0 0 1 9.2 0c0 2.8.8 4.1 1.4 4.8H4c.6-.7 1.4-2 1.4-4.8Z" />
    <path d="M8.3 16a1.8 1.8 0 0 0 3.4 0" />
  {:else if name === "plus"}
    <path d="M10 4.5v11M4.5 10h11" />
  {:else if name === "more"}
    <circle cx="4.4" cy="10" r="1.15" fill="currentColor" stroke="none" />
    <circle cx="10" cy="10" r="1.15" fill="currentColor" stroke="none" />
    <circle cx="15.6" cy="10" r="1.15" fill="currentColor" stroke="none" />
  {:else if name === "close"}
    <path d="m5.5 5.5 9 9M14.5 5.5l-9 9" />
  {:else if name === "chevron"}
    <path d="m5.5 8 4.5 4.5L14.5 8" />
  {:else if name === "external"}
    <path d="M11.5 4.5H16v4.5" />
    <path d="M16 4.5 9.8 10.7" />
    <path d="M15.5 12v2.5a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1v-9a1 1 0 0 1 1-1H8" />
  {:else if name === "copy"}
    <rect x="7" y="7" width="8.5" height="8.5" rx="1.6" />
    <path d="M13 7V5.6a1.1 1.1 0 0 0-1.1-1.1H5.6A1.1 1.1 0 0 0 4.5 5.6v6.3A1.1 1.1 0 0 0 5.6 13H7" />
  {:else if name === "refresh"}
    <path d="M15.6 8.2A6 6 0 1 0 16 11.4" />
    <path d="M15.8 4.4v3.9h-3.9" />
  {:else if name === "play"}
    <path d="M7 5.4 14.4 10 7 14.6Z" />
  {:else if name === "stop"}
    <rect x="6" y="6" width="8" height="8" rx="1.4" />
  {:else if name === "search"}
    <circle cx="9" cy="9" r="4.7" />
    <path d="m12.6 12.6 3 3" />
  {:else if name === "warning"}
    <path d="M10 3.4 17 16H3Z" />
    <path d="M10 8.1v3.5" />
    <path d="M10 14.2v.1" />
  {:else if name === "user"}
    <circle cx="10" cy="7" r="2.6" />
    <path d="M4.8 16.3c0-2.7 2.3-4.5 5.2-4.5s5.2 1.8 5.2 4.5" />
  {:else if name === "info"}
    <circle cx="10" cy="10" r="6.6" />
    <path d="M10 9.2v4" />
    <path d="M10 6.6v.1" />
  {:else if name === "clock"}
    <circle cx="10" cy="10" r="6.6" />
    <path d="M10 6.2V10l2.6 1.6" />
  {:else if name === "folder"}
    <path d="M3.5 6.4a1 1 0 0 1 1-1h3l1.4 1.6h6.6a1 1 0 0 1 1 1v6.6a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1Z" />
  {:else if name === "file"}
    <path d="M6 3.5h5l3.5 3.5v9.5H6Z" />
    <path d="M11 3.5V7h3.5" />
  {:else if name === "plug"}
    <path d="M8 3.5v3M12 3.5v3" />
    <path d="M6 6.5h8v3.1a4 4 0 0 1-8 0Z" />
    <path d="M10 13.8v2.7" />
  {:else if name === "trash"}
    <path d="M4.5 6.2h11M8 6.2V4.6h4v1.6M6 6.2l.7 9.1h6.6l.7-9.1" />
  {:else if name === "power"}
    <path d="M10 3.4v6.1" />
    <path d="M6.3 5.8a5.4 5.4 0 1 0 7.4 0" />
  {:else if name === "panel-left"}
    <rect x="3.4" y="4.2" width="13.2" height="11.6" rx="2.4" />
    <path d="M8.1 4.2v11.6" />
  {:else if name === "win-minimize"}
    <path d="M4.5 10h11" />
  {:else if name === "win-maximize"}
    <rect x="4.8" y="4.8" width="10.4" height="10.4" rx="1" />
  {:else if name === "win-restore"}
    <rect x="6.4" y="6.4" width="8.8" height="8.8" rx="1" />
    <path d="M6.4 6.4V5.4a1 1 0 0 1 1-1h7.2a1 1 0 0 1 1 1v7.2a1 1 0 0 1-1 1h-1" />
  {:else if name === "win-close"}
    <path d="m5.4 5.4 9.2 9.2M14.6 5.4l-9.2 9.2" />
  {:else if name === "arrow-left"}
    <path d="M15.5 10H4.5M8.8 5.7 4.5 10l4.3 4.3" />
  {/if}
</svg>
