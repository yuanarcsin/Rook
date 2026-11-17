// 动效时长：统一 150–220ms，并尊重系统「减少动态效果」偏好。
//
// 只用于 Svelte 内置 transition（fade/fly 等）；纯 CSS 过渡由 app.css 的
// @media (prefers-reduced-motion: reduce) 兜底，二者保持同一套时长。

export const MOTION_FAST = 150;
export const MOTION_BASE = 180;
export const MOTION_SLOW = 220;

/** 系统开启「减少动态效果」时返回 0，让 transition 立即完成。 */
export function motionDuration(ms: number): number {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
    return ms;
  }
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches ? 0 : ms;
}
