// 弹窗栈：只让最上层弹窗响应 Esc / Tab 焦点锁定，并统一管理背景滚动锁。
//
// 场景：详情弹窗里再打开「停止确认」、任务编辑器里再打开「删除确认」。
// 若每个 Modal 都直接监听 window，Esc 会把两层一起关掉；滚动锁也会被
// 内层提前恢复。

type ModalEntry = { id: symbol; card: HTMLElement | null };

const stack: ModalEntry[] = [];
let savedOverflow: string | null = null;

export function pushModal(id: symbol, card: HTMLElement | null) {
  if (stack.length === 0 && typeof document !== "undefined") {
    savedOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
  }
  stack.push({ id, card });
}

export function popModal(id: symbol) {
  for (let index = stack.length - 1; index >= 0; index -= 1) {
    if (stack[index].id === id) {
      stack.splice(index, 1);
      break;
    }
  }
  if (stack.length === 0 && savedOverflow !== null && typeof document !== "undefined") {
    document.body.style.overflow = savedOverflow;
    savedOverflow = null;
  }
}

export function isTopModal(id: symbol): boolean {
  return stack.length > 0 && stack[stack.length - 1].id === id;
}

export function hasModal(): boolean {
  return stack.length > 0;
}

/** 焦点目标是否仍在当前最上层弹窗内。 */
export function topModalContains(node: Node | null): boolean {
  const card = stack.length > 0 ? stack[stack.length - 1].card : null;
  return node !== null && card !== null && card.contains(node);
}

/** 关闭内层弹窗后，把焦点交给剩余的最上层弹窗。 */
export function focusTopModal() {
  if (stack.length > 0) {
    stack[stack.length - 1].card?.focus();
  }
}
