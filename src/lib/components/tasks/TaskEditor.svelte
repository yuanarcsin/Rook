<script lang="ts">
  // 任务编辑器：基本/运行常驻，其余字段收进「高级设置」。
  // 业务状态在 state/task-editor.svelte.ts；这里只负责模板与关闭拦截。
  import { onMount } from "svelte";
  import { BRAND } from "$lib/brand";
  import { app } from "$lib/state/app.svelte";
  import { TaskEditorState } from "$lib/state/task-editor.svelte";
  import { draftStageLabel } from "$lib/utils/task-draft-form";
  import { ELEVATED_NOTICE } from "$lib/utils/task-utils";
  import ConfirmDialog from "../ui/ConfirmDialog.svelte";
  import Icon from "../ui/Icon.svelte";
  import Overlay from "../ui/Overlay.svelte";

  let editor = new TaskEditorState();
  let confirmDelete = $state(false);
  let confirmDiscard = $state(false);
  let advancedOpen = $state(false);

  onMount(() => {
    void editor.load();
  });

  function onAdvancedToggle(event: Event) {
    advancedOpen = (event.currentTarget as HTMLDetailsElement).open;
  }

  /** 有未保存修改时先确认；未修改直接关闭。 */
  function requestClose() {
    if (editor.saving || editor.deleting) return;
    if (editor.dirty) confirmDiscard = true;
    else app.closeEditor();
  }

  function discardAndClose() {
    confirmDiscard = false;
    app.closeEditor();
  }

  function requestDelete() {
    confirmDelete = true;
  }

  async function confirmDeleteTask() {
    confirmDelete = false;
    await editor.remove();
  }
</script>

{#snippet footer()}
  {#if editor.mode === "edit"}
    <button
      type="button"
      class="btn btn-danger"
      onclick={requestDelete}
      disabled={editor.saving || editor.deleting}
    >
      删除脚本
    </button>
    <span class="grow"></span>
  {/if}
  <button type="button" class="btn btn-ghost" onclick={requestClose} disabled={editor.saving || editor.deleting}>
    取消
  </button>
  {#if editor.conflict}
    <button type="button" class="btn btn-ghost" onclick={() => void editor.reload()} disabled={editor.loading}>
      重新加载
    </button>
  {/if}
  <button
    type="button"
    class="btn btn-primary"
    onclick={() => void editor.save()}
    disabled={editor.saving || editor.deleting}
  >
    {editor.saving ? "保存中…" : editor.mode === "edit" ? "保存修改" : "创建脚本"}
  </button>
{/snippet}

<Overlay
  title={editor.mode === "edit" ? "编辑脚本" : "新建脚本"}
  subtitle={editor.mode === "edit" ? "改动会写回配置文件，保留注释与其他脚本。" : "创建后会写回配置文件；默认不会自动启动。"}
  wide
  onclose={requestClose}
  footer={footer}
>
  {#if editor.loading}
    <p class="detail-note">读取草稿中…</p>
  {:else}
    <div class="form-section">
      <h3 class="detail-section-title">基本</h3>
      <div class="form-grid">
        <label class="field" title="唯一标识；保存后不可修改">
          <span>id</span>
          <input
            type="text"
            bind:value={editor.form.id}
            readonly={editor.mode === "edit"}
            placeholder="例如 ykt-gateway"
          />
        </label>
        <label class="field">
          <span>名称</span>
          <input type="text" bind:value={editor.form.name} placeholder="显示名称" />
        </label>
        <div class="checks wide">
          <label class="check" title="禁用后不会自动启动，运行时段也不会拉起">
            <input type="checkbox" bind:checked={editor.form.enabled} />
            启用
          </label>
          <label class="check" title={"下次启动 " + BRAND.name + " 时生效；启用运行时段后由时段接管"}>
            <input type="checkbox" bind:checked={editor.form.autoStart} />
            随应用启动
          </label>
          <label class="check" title="需要人工处理时在窗口内提示">
            <input type="checkbox" bind:checked={editor.form.notify} />
            通知
          </label>
        </div>
      </div>
    </div>

    <div class="form-section">
      <h3 class="detail-section-title">运行</h3>
      <div class="form-grid">
        <label class="field wide" title="command：可执行文件路径">
          <span>命令</span>
          <input type="text" bind:value={editor.form.command} placeholder="可执行文件路径，例如 node" />
        </label>
        <label class="field wide" title="args：每行一个参数">
          <span>参数</span>
          <textarea bind:value={editor.form.args} rows="3" spellcheck="false"></textarea>
        </label>
        <label class="field wide" title="working_dir：脚本所在目录，必填">
          <span>工作目录</span>
          <input type="text" bind:value={editor.form.workingDir} placeholder="例如 D:/scripts/ykt" />
        </label>
        <label class="field" title="ports：逗号分隔">
          <span>端口</span>
          <input type="text" bind:value={editor.form.ports} placeholder="3000, 3001" />
        </label>
        <label class="field" title="env_proxy：传给脚本的代理策略">
          <span>代理</span>
          <select bind:value={editor.form.envProxy}>
            <option value="bypass_localhost">本机直连</option>
            <option value="inherit">继承系统</option>
            <option value="strip">移除代理</option>
          </select>
        </label>
        <label class="field wide" title="env：每行 KEY=VALUE；敏感信息建议由脚本自己保存">
          <span>环境变量</span>
          <textarea bind:value={editor.form.env} rows="4" spellcheck="false"></textarea>
        </label>
        <label class="field wide" title="panel_url：控制台地址，可留空">
          <span>控制台地址</span>
          <input type="text" bind:value={editor.form.panelUrl} placeholder="http://127.0.0.1:3000/" />
        </label>
      </div>
    </div>

    <details class="details-block" open={advancedOpen} ontoggle={onAdvancedToggle}>
      <summary>
        高级设置
        <span class="chevron"><Icon name="chevron" size={14} /></span>
      </summary>
      <div class="details-body">
        <div class="form-section">
          <h3 class="detail-section-title">健康检查</h3>
          <div class="form-grid">
            <label class="field" title="health.type">
              <span>类型</span>
              <select bind:value={editor.form.health.kind}>
                <option value="process">进程存活</option>
                <option value="http">HTTP 探测</option>
                <option value="heartbeat">脚本心跳</option>
              </select>
            </label>
            <label class="field" title="health.interval_ms">
              <span>间隔 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.health.intervalMs} />
            </label>
            <label class="field" title="health.timeout_ms">
              <span>超时 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.health.timeoutMs} />
            </label>
            <label class="field" title="health.startup_grace_ms">
              <span>启动宽限 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.health.startupGraceMs} />
            </label>
            <label class="field" title="health.failure_threshold">
              <span>失败阈值</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.health.failureThreshold} />
            </label>
            {#if editor.form.health.kind === "http"}
              <label class="field wide" title="health.url">
                <span>探测地址</span>
                <input type="text" bind:value={editor.form.health.url} placeholder="http://127.0.0.1:3000/health" />
              </label>
              <label class="field" title="health.expect_status：逗号分隔">
                <span>期望状态码</span>
                <input type="text" bind:value={editor.form.health.expectStatus} placeholder="200" />
              </label>
              <label class="field wide" title="health.expect_body_contains">
                <span>响应体包含</span>
                <input type="text" bind:value={editor.form.health.expectBodyContains} />
              </label>
            {:else if editor.form.health.kind === "heartbeat"}
              <label class="field wide" title="health.path：留空使用默认路径">
                <span>心跳文件</span>
                <input type="text" bind:value={editor.form.health.path} />
              </label>
              <label class="field" title="health.max_age_ms：heartbeat 必填">
                <span>最大年龄 (ms)</span>
                <input type="text" inputmode="numeric" bind:value={editor.form.health.maxAgeMs} />
              </label>
              <label class="field wide" title="health.needs_human_status：逗号分隔">
                <span>需要人工的状态</span>
                <input type="text" bind:value={editor.form.health.needsHumanStatus} placeholder="login_required, blocked" />
              </label>
            {:else}
              <p class="field-hint wide">进程存活只能粗粒度监督：进程在即健康。</p>
            {/if}
          </div>
        </div>

        <div class="form-section">
          <h3 class="detail-section-title">重启策略</h3>
          <p class="field-hint">on-failure 异常退出才重启；always 退出就重启；never 不重启。</p>
          <div class="form-grid">
            <label class="field" title="restart.mode">
              <span>模式</span>
              <select bind:value={editor.form.restart.mode}>
                <option value="on-failure">on-failure</option>
                <option value="always">always</option>
                <option value="never">never</option>
              </select>
            </label>
            <label class="field" title="restart.initial_delay_ms">
              <span>初始延迟 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.initialDelayMs} />
            </label>
            <label class="field" title="restart.max_delay_ms">
              <span>最大延迟 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.maxDelayMs} />
            </label>
            <label class="field" title="restart.multiplier">
              <span>倍率</span>
              <input type="text" inputmode="decimal" bind:value={editor.form.restart.multiplier} />
            </label>
            <label class="field" title="restart.jitter">
              <span>抖动</span>
              <input type="text" inputmode="decimal" bind:value={editor.form.restart.jitter} />
            </label>
            <label class="field" title="restart.max_consecutive_failures">
              <span>最大连续失败</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.maxConsecutiveFailures} />
            </label>
            <label class="field" title="restart.pause_on_exit_codes：逗号分隔">
              <span>暂停退出码</span>
              <input type="text" bind:value={editor.form.restart.pauseOnExitCodes} />
            </label>
            <label class="field" title="restart.stable_after_ms">
              <span>稳定窗口 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.stableAfterMs} />
            </label>
            <label class="field" title="restart.window_ms：启动窗口长度，0 = 不限">
              <span>启动窗口 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.windowMs} />
            </label>
            <label class="field" title="restart.max_starts_in_window：窗口内最多启动次数，0 = 不限">
              <span>窗口内最多启动</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.restart.maxStartsInWindow} />
            </label>
          </div>
        </div>

        <div class="form-section">
          <h3 class="detail-section-title">停止</h3>
          <p class="field-hint">优雅停止先请求脚本自行退出，宽限内未退出再强制结束；立即终止会直接杀掉进程树。</p>
          <div class="form-grid">
            <label class="field" title="stop.method">
              <span>方式</span>
              <select bind:value={editor.form.stop.method}>
                <option value="graceful">优雅停止</option>
                <option value="terminate">立即终止</option>
              </select>
            </label>
            <label class="field" title="stop.grace_ms：100–60000">
              <span>宽限 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.stop.graceMs} />
            </label>
            <label class="field" title="stop.timeout_ms">
              <span>超时 (ms)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.stop.timeoutMs} />
            </label>
          </div>
        </div>

        <div class="form-section">
          <h3 class="detail-section-title">日志</h3>
          <div class="form-grid">
            <label class="field wide" title="logs.dir：留空使用默认目录">
              <span>日志目录</span>
              <input type="text" bind:value={editor.form.logs.dir} />
            </label>
            <label class="field" title="logs.rotation">
              <span>轮转</span>
              <select bind:value={editor.form.logs.rotation}>
                <option value="size">按大小</option>
                <option value="daily">按天</option>
                <option value="none">不轮转</option>
              </select>
            </label>
            <label class="field" title="logs.encoding">
              <span>编码</span>
              <select bind:value={editor.form.logs.encoding}>
                <option value="auto">自动</option>
                <option value="utf-8">UTF-8</option>
                <option value="gb18030">GB18030</option>
              </select>
            </label>
            <label class="field" title="logs.max_file_mb">
              <span>单文件上限 (MB)</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.logs.maxFileMb} />
            </label>
            <label class="field" title="logs.max_files">
              <span>最大文件数</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.logs.maxFiles} />
            </label>
            <label class="field" title="logs.max_days">
              <span>保留天数</span>
              <input type="text" inputmode="numeric" bind:value={editor.form.logs.maxDays} />
            </label>
          </div>
        </div>

        <div class="form-section">
          <h3 class="detail-section-title">电源</h3>
          <div class="form-grid">
            <label class="field" title="power.resume_grace_ms：留空继承全局">
              <span>唤醒宽限 (ms)</span>
              <input
                type="text"
                inputmode="numeric"
                bind:value={editor.form.power.resumeGraceMs}
                placeholder="留空继承全局"
              />
            </label>
          </div>
        </div>

        <div class="form-section">
          <h3 class="detail-section-title">权限</h3>
          <div class="checks">
            <label class="check disabled" title={ELEVATED_NOTICE}>
              <input type="checkbox" checked={editor.form.elevated} disabled />
              管理员权限（提权未实现）
            </label>
            <span class="tag tag-warn">暂不可用</span>
          </div>
          <p class="field-hint">勾选不会改变实际权限，脚本仍以普通权限运行。</p>
          {#if editor.form.elevated}
            <div class="row">
              <button
                type="button"
                class="btn btn-sm btn-ghost"
                onclick={() => (editor.form.elevated = false)}
              >
                清除 elevated 标记
              </button>
            </div>
          {/if}
        </div>
      </div>
    </details>

    {#if editor.error && !editor.detail}
      <p class="field-error" role="alert">{editor.error}</p>
    {/if}
    {#if editor.detail}
      <div class="callout callout-danger">
        <div class="callout-title">{editor.detail.message}</div>
        <p>
          阶段：{draftStageLabel[editor.detail.stage] ?? editor.detail.stage}
          {#if editor.detail.line !== null}
            · 第 {editor.detail.line} 行{editor.detail.col !== null ? ":" + editor.detail.col : ""}
          {/if}
          {#if editor.detail.currentRevision}
            · 磁盘 revision {editor.detail.currentRevision.slice(0, 12)}
          {/if}
        </p>
      </div>
    {/if}
  {/if}
</Overlay>

{#if confirmDelete}
  <ConfirmDialog
    title="删除脚本"
    message={"删除「" + editor.taskId + "」会停止运行中的实例并删除配置，不可撤销。日志文件默认保留。"}
    confirmLabel="删除"
    tone="danger"
    busy={editor.deleting}
    onconfirm={() => void confirmDeleteTask()}
    oncancel={() => (confirmDelete = false)}
  />
{/if}

{#if confirmDiscard}
  <ConfirmDialog
    title="放弃未保存的修改？"
    message="当前表单有未保存的修改，关闭后这些内容会丢失。"
    confirmLabel="放弃修改"
    cancelLabel="继续编辑"
    tone="danger"
    onconfirm={discardAndClose}
    oncancel={() => (confirmDiscard = false)}
  />
{/if}
