<script lang="ts">
  // 全局提示：只保留“配置写错”和托盘提示这两个需要就地操作的横幅；
  // 配置 warning 由 app.syncConfigWarnings() 收进通知中心，不再占页面。
  import { acknowledgeTrayHint } from "$lib/api";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import { backupFileName, reloadFromDisk, restoreBackup } from "$lib/services/config";
  import { errorMessage } from "$lib/utils/task-utils";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";

  let trayHint = $state(false);
  let trayHintHandled = false;
  let confirmRestore = $state(false);
  let restoring = $state(false);
  let restoreError = $state<string | null>(null);

  $effect(() => {
    if (trayHintHandled || !app.snapshot?.trayHintPending) return;
    trayHintHandled = true;
    trayHint = true;
    // 展示即消费标记：下次关窗后不会再提示。
    void acknowledgeTrayHint()
      .then((snapshot) => app.applySnapshot(snapshot))
      .catch(() => {});
  });

  async function doRestore() {
    confirmRestore = false;
    restoring = true;
    restoreError = null;
    try {
      await restoreBackup();
    } catch (failure) {
      restoreError = errorMessage(failure);
    } finally {
      restoring = false;
    }
  }

</script>

{#if app.snapshot?.configError}
  {@const configError = app.snapshot.configError}
  <div class="callout callout-danger" role="alert">
    <div class="callout-title">配置文件有一处写错了，这次修改没有生效</div>
    <p>正在运行的脚本不受影响。改好后点「重新加载」即可。</p>
    <details class="banner-detail">
      <summary>技术细节</summary>
      <p class="location">{configError.file}:{configError.line}:{configError.col}</p>
      {#if configError.path}<p>字段：{configError.path}</p>{/if}
      <p>{configError.message}</p>
    </details>
    <div class="callout-actions">
      <button type="button" class="btn btn-sm" onclick={() => app.openConfig()}>打开配置</button>
      <button type="button" class="btn btn-sm" onclick={() => void reloadFromDisk()}>重新加载</button>
      <button
        type="button"
        class="btn btn-sm"
        title="用配置文件旁的 .bak 覆盖当前配置"
        onclick={() => (confirmRestore = true)}
        disabled={restoring}
      >
        {restoring ? "恢复中…" : "恢复上一版"}
      </button>
    </div>
    {#if restoreError}<p>{restoreError}</p>{/if}
  </div>
{/if}

{#if trayHint}
  <div class="callout callout-info" role="status">
    <div class="callout-title">{BRAND.name} 仍在托盘运行</div>
    <p>关闭窗口不会退出。可以从托盘菜单恢复窗口，或在左下角「退出」结束应用（会先停止全部脚本）。</p>
    <div class="callout-actions">
      <button type="button" class="btn btn-sm" onclick={() => (trayHint = false)}>知道了</button>
    </div>
  </div>
{/if}

{#if confirmRestore}
  <ConfirmDialog
    title="恢复上一版配置"
    message={"将用 " + backupFileName() + " 覆盖当前配置；当前（可能已损坏的）文件会另存为 .bak.bad，恢复后立即重新加载。"}
    confirmLabel="恢复并重新加载"
    onconfirm={() => void doRestore()}
    oncancel={() => (confirmRestore = false)}
  />
{/if}
