<script lang="ts">
  // 设置：常规、文件、维护三组。所有状态只读展示，写入动作都指向明确按钮。
  import { onMount } from "svelte";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import { reloadFromDisk } from "$lib/services/config";
  import {
    exportDiagnosticsPackage,
    loadAutostartStatus,
    loadToastStatus,
    openDirectory,
    sendTestToast,
    type Loaded,
  } from "$lib/services/system";
  import type { AutostartStatus, DiagnosticsResult, ToastStatus } from "$lib/api";
  import { formatTimestamp } from "$lib/utils/format";
  import {
    autostartLabel,
    autostartModeLabel,
    copyText,
    errorMessage,
  } from "$lib/utils/task-utils";
  import GlobalBanners from "../shell/GlobalBanners.svelte";
  import PageHeader from "../shell/PageHeader.svelte";

  let autostart = $state<Loaded<AutostartStatus>>({ status: null, error: null });
  let autostartLoading = $state(true);
  let toast = $state<Loaded<ToastStatus>>({ status: null, error: null });
  let toastBusy = $state(false);
  let toastNotice = $state<string | null>(null);
  let copied = $state<string | null>(null);
  let openError = $state<string | null>(null);
  let reloadError = $state<string | null>(null);
  let diagnostics = $state<DiagnosticsResult | null>(null);
  let diagnosticsError = $state<string | null>(null);
  let diagnosticsBusy = $state(false);

  const watchLabel: Record<string, string> = {
    watching: "监听中",
    polling: "回退轮询",
    disabled: "已关闭",
    error: "监听异常",
  };

  const themeOptions = [
    { value: "system", label: "跟随系统" },
    { value: "light", label: "浅色" },
    { value: "dark", label: "深色" },
  ] as const;

  let watch = $derived(app.snapshot?.configWatch ?? null);

  onMount(() => {
    void refreshAutostart();
    void refreshToast();
  });

  async function refreshAutostart() {
    autostartLoading = true;
    autostart = await loadAutostartStatus();
    autostartLoading = false;
  }

  async function refreshToast() {
    toast = await loadToastStatus();
  }

  async function testNotify() {
    if (toastBusy) return;
    toastBusy = true;
    toastNotice = null;
    const result = await sendTestToast();
    toast = result;
    if (result.status?.available) {
      toastNotice = "已发送一条测试通知。";
    }
    toastBusy = false;
  }

  async function copy(text: string, label: string) {
    if (text.length === 0) return;
    copied = (await copyText(text))
      ? "已复制" + label + "。"
      : "复制" + label + "失败，请手动选择后复制。";
  }

  async function open(kind: "logs" | "data" | "config_dir") {
    openError = null;
    try {
      await openDirectory(kind);
    } catch (failure) {
      openError = errorMessage(failure);
    }
  }

  async function reload() {
    reloadError = null;
    try {
      await reloadFromDisk();
    } catch (failure) {
      reloadError = errorMessage(failure);
    }
  }

  async function exportDiagnostics() {
    if (diagnosticsBusy) return;
    diagnosticsBusy = true;
    diagnosticsError = null;
    try {
      diagnostics = await exportDiagnosticsPackage();
    } catch (failure) {
      diagnostics = null;
      diagnosticsError = errorMessage(failure);
    } finally {
      diagnosticsBusy = false;
    }
  }

  function openEvents() {
    app.logSegment = "event";
    app.setView("logs");
  }
</script>

<div class="page page-narrow">
  <PageHeader title="设置" lead="所有设置都保存在本地配置文件里；这里只展示状态与入口。" />
  <GlobalBanners />

  {#if copied}<p class="field-hint" role="status">{copied}</p>{/if}
  {#if openError}<p class="field-error" role="alert">打开目录失败：{openError}</p>{/if}
  {#if reloadError}<p class="field-error" role="alert">重新加载失败：{reloadError}</p>{/if}

  <div class="settings-stack">
    <section class="group" aria-label="常规">
      <div class="group-head">
        <h2 class="group-title">常规</h2>
        <span class="group-note">外观、启动与通知</span>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">外观</div>
          <div class="setting-desc">跟随系统，或固定浅色/深色；只影响本机界面，不写入配置文件。</div>
        </div>
        <div class="setting-control">
          <div class="segmented" role="group" aria-label="外观主题">
            {#each themeOptions as option (option.value)}
              <button
                type="button"
                aria-pressed={app.themePref === option.value}
                onclick={() => app.setThemePref(option.value)}
              >
                {option.label}
              </button>
            {/each}
          </div>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">
            开机自启
            {#if !autostartLoading}
              <span class="tag">
                {autostart.error
                  ? "读取失败"
                  : autostart.status
                    ? autostartLabel(autostart.status)
                    : "—"}
              </span>
            {/if}
          </div>
          <div class="setting-desc">
            {#if autostart.error}
              {autostart.error}
            {:else if autostart.status}
              {autostartModeLabel[autostart.status.mode] ?? autostart.status.mode}
              {#if autostart.status.expectedValue}
                · <code class="path-text">{autostart.status.expectedValue}</code>
              {/if}
            {:else}
              只读注册表状态；启动流程按配置同步 Run 键。
            {/if}
          </div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void refreshAutostart()} disabled={autostartLoading}>
            {autostartLoading ? "读取中…" : "刷新"}
          </button>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">
            系统通知
            {#if toast.status}
              <span class="tag {toast.status.available ? "tag-ok" : "tag-bad"}">
                {toast.status.available ? "可用" : "不可用"}
              </span>
            {/if}
          </div>
          <div class="setting-desc">
            {#if toast.error}
              {toast.error}
            {:else if toast.status}
              {#if toast.status.available}
                使用开始菜单快捷方式作为通知身份；缺失时自动降级为应用内提示。
              {:else}
                {toast.status.reason ?? "系统通知不可用"}
              {/if}
            {:else}
              读取中…
            {/if}
            {#if toastNotice} · {toastNotice}{/if}
          </div>
        </div>
        <div class="setting-control">
          <button
            type="button"
            class="btn btn-sm btn-secondary"
            onclick={() => void testNotify()}
            disabled={toastBusy || toast.status === null}
          >
            {toastBusy ? "发送中…" : "发送测试通知"}
          </button>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">
            配置自动重载
            <span class="tag">{watch ? watchLabel[watch.state] ?? watch.state : "—"}</span>
          </div>
          <div class="setting-desc">
            {#if watch}
              watch_config = {watch.enabled ? "true" : "false"} · 上次重载 {formatTimestamp(watch.lastReloadAt)}
              {#if watch.lastError}
                · <span class="tone-bad">{watch.lastError.file}:{watch.lastError.line} {watch.lastError.message}</span>
              {/if}
            {:else}
              读取中…
            {/if}
          </div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-secondary" onclick={() => void reload()} disabled={app.busy}>
            {app.busy ? "重载中…" : "重新加载"}
          </button>
        </div>
      </div>
    </section>

    <section class="group" aria-label="文件">
      <div class="group-head">
        <h2 class="group-title">文件</h2>
        <span class="group-note">配置、数据与日志目录</span>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">配置文件</div>
          <div class="setting-desc"><code class="path-text">{app.snapshot?.configPath ?? "—"}</code></div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-primary" onclick={() => app.openConfig()}>编辑</button>
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void open("config_dir")}>打开</button>
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void copy(app.snapshot?.configPath ?? "", "配置路径")}>复制</button>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">数据目录</div>
          <div class="setting-desc"><code class="path-text">{app.snapshot?.dataDir ?? "—"}</code></div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void open("data")}>打开</button>
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void copy(app.snapshot?.dataDir ?? "", "数据目录路径")}>复制</button>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">脚本日志根目录</div>
          <div class="setting-desc">每个脚本一个子目录，保留天数由脚本配置决定。</div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-ghost" onclick={() => void open("logs")}>打开</button>
        </div>
      </div>
    </section>

    <section class="group" aria-label="维护">
      <div class="group-head">
        <h2 class="group-title">维护</h2>
        <span class="group-note">版本、诊断与事件</span>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">版本</div>
          <div class="setting-desc">{BRAND.name} {app.snapshot?.appVersion ?? "—"} · {BRAND.tagline}</div>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">导出诊断包</div>
          <div class="setting-desc">
            版本、配置与日志打包到数据目录的 diagnostics/。分享前请检查敏感信息。
            {#if diagnostics}
              · 已导出 {diagnostics.files} 个文件到 <code class="path-text">{diagnostics.path}</code>
            {/if}
            {#if diagnosticsError} · <span class="tone-bad">{diagnosticsError}</span>{/if}
          </div>
        </div>
        <div class="setting-control">
          {#if diagnostics}
            <button type="button" class="btn btn-sm btn-ghost" onclick={() => void copy(diagnostics?.path ?? "", "导出路径")}>复制路径</button>
          {/if}
          <button type="button" class="btn btn-sm btn-secondary" onclick={() => void exportDiagnostics()} disabled={diagnosticsBusy}>
            {diagnosticsBusy ? "导出中…" : "导出诊断包"}
          </button>
        </div>
      </div>

      <div class="setting-row">
        <div class="setting-text">
          <div class="setting-title">应用事件</div>
          <div class="setting-desc">JSONL，最近 200 行；可以只看状态迁移。</div>
        </div>
        <div class="setting-control">
          <button type="button" class="btn btn-sm btn-secondary" onclick={openEvents}>查看事件</button>
        </div>
      </div>
    </section>
  </div>
</div>
