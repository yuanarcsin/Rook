<script lang="ts">
  // 运行时段编辑器：唯一开关 + 窗口表。进入自动启动一次，结束停止任何实例。
  import { onMount } from "svelte";
  import {
    getSchedule,
    saveSchedule,
    type ScheduleDraft,
    type ScheduleView,
    type ScheduleWindowDraft,
  } from "$lib/api";
  import { app } from "$lib/state/app.svelte";
  import { formatWhen } from "$lib/utils/format";
  import { describeSchedule, validateSchedule, WEEKDAY_OPTIONS } from "$lib/utils/schedule";
  import { errorMessage } from "$lib/utils/task-utils";

  let { taskId, initial }: { taskId: string; initial: ScheduleView } = $props();

  const weekdayOrder = WEEKDAY_OPTIONS.map((day) => day.value);

  function draftFrom(view: ScheduleDraft | ScheduleView): ScheduleDraft {
    return {
      enabled: view.enabled,
      windows: view.windows.map((window) => ({
        days: [...window.days],
        start: window.start,
        end: window.end,
      })),
    };
  }

  const EMPTY: ScheduleDraft = { enabled: false, windows: [] };
  let schedule = $state<ScheduleDraft>({ enabled: false, windows: [] });
  let baseline = $state(JSON.stringify(EMPTY));
  let revision = $state("");
  let loaded = $state(false);
  let loading = $state(true);
  let saving = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let conflict = $state(false);

  let dirty = $derived(JSON.stringify(schedule) !== baseline);

  let statusText = $derived.by(() => {
    if (!initial.enabled) return "未启用";
    if (initial.inWindow && initial.nextEndAt !== null) {
      return "时段内 · " + formatWhen(initial.nextEndAt) + " 结束";
    }
    if (!initial.inWindow && initial.nextStartAt !== null) {
      return "下次 " + formatWhen(initial.nextStartAt);
    }
    return describeSchedule(initial.windows);
  });

  onMount(() => {
    schedule = draftFrom(initial);
    baseline = JSON.stringify(schedule);
    void load();
  });

  async function load() {
    loading = true;
    error = null;
    conflict = false;
    notice = null;
    try {
      const view = await getSchedule(taskId);
      schedule = draftFrom(view.schedule);
      baseline = JSON.stringify(schedule);
      revision = view.revision;
      loaded = true;
    } catch (failure) {
      error = errorMessage(failure);
    } finally {
      loading = false;
    }
  }

  async function save() {
    if (saving || loading || !loaded) return;
    const invalid = validateSchedule(schedule);
    if (invalid) {
      error = invalid;
      return;
    }
    saving = true;
    error = null;
    conflict = false;
    notice = null;
    try {
      // 传给 IPC 的必须是普通对象：Svelte $state 是代理，避免序列化差异。
      const payload: ScheduleDraft = {
        enabled: schedule.enabled,
        windows: schedule.windows.map((window) => ({
          days: [...window.days],
          start: window.start,
          end: window.end,
        })),
      };
      const result = await saveSchedule(taskId, payload, revision);
      revision = result.revision;
      app.applySnapshot(result.snapshot);
      baseline = JSON.stringify(schedule);
      notice = result.changed ? "已保存" : "没有变化";
    } catch (failure) {
      const detail = failure as { stage?: string } | null;
      if (detail && detail.stage === "conflict") {
        conflict = true;
        error = "配置已被外部修改，未写入。请重新读取后再保存。";
      } else {
        error = errorMessage(failure);
      }
    } finally {
      saving = false;
    }
  }

  function toggleDay(window: ScheduleWindowDraft, day: string) {
    if (window.days.includes(day)) {
      window.days = window.days.filter((item) => item !== day);
    } else {
      window.days = [...window.days, day].sort(
        (left, right) => weekdayOrder.indexOf(left) - weekdayOrder.indexOf(right),
      );
    }
  }

  function addWindow() {
    schedule.windows = [
      ...schedule.windows,
      { days: ["mon", "tue", "wed", "thu", "fri"], start: "08:00", end: "18:00" },
    ];
  }

  function removeWindow(index: number) {
    schedule.windows = schedule.windows.filter((_, item) => item !== index);
  }

  function isCrossMidnight(window: ScheduleWindowDraft): boolean {
    return window.start !== "" && window.end !== "" && window.start > window.end;
  }

  function dayLabel(day: string): string {
    return WEEKDAY_OPTIONS.find((item) => item.value === day)?.label ?? day;
  }
</script>

<div class="detail-section-head">
  <span class="detail-section-title">运行时段</span>
  <span class="detail-note">{statusText}</span>
</div>

{#if loading}
  <p class="detail-note">读取中…</p>
{:else}
  <label class="toggle">
    <input type="checkbox" bind:checked={schedule.enabled} disabled={saving} />
    <span class="toggle-track"></span>
    启用运行时段
  </label>
  <p class="schedule-hint">进入时段自动启动一次；时段结束停止该脚本的任何实例。</p>

  {#if schedule.enabled}
    <div class="schedule-windows">
      {#each schedule.windows as window, index (index)}
        <div class="schedule-window">
          <div class="schedule-row">
            <strong>时段 {index + 1}</strong>
            <button
              type="button"
              class="btn-link"
              onclick={() => removeWindow(index)}
              disabled={saving}
            >
              删除
            </button>
          </div>
          <div class="weekday-chips" role="group" aria-label="生效星期">
            {#each WEEKDAY_OPTIONS as day (day.value)}
              <button
                type="button"
                class="chip"
                aria-pressed={window.days.includes(day.value)}
                aria-label={day.label}
                onclick={() => toggleDay(window, day.value)}
                disabled={saving}
              >
                {day.short}
              </button>
            {/each}
          </div>
          <div class="time-grid">
            <label class="field">
              <span>开始</span>
              <input type="time" bind:value={window.start} disabled={saving} />
            </label>
            <label class="field">
              <span>结束</span>
              <input type="time" bind:value={window.end} disabled={saving} />
            </label>
          </div>
          <p class="schedule-hint">
            {#if window.days.length === 0}
              未选择星期
            {:else}
              生效：{window.days.map(dayLabel).join("、")}
            {/if}
            {#if isCrossMidnight(window)} · 跨午夜{/if}
          </p>
        </div>
      {/each}
    </div>
  {/if}

  <div class="schedule-actions">
    {#if schedule.enabled}
      <button type="button" class="btn btn-sm btn-ghost" onclick={addWindow} disabled={saving}>
        添加时段
      </button>
    {/if}
    {#if dirty}
      <button
        type="button"
        class="btn btn-sm btn-primary"
        onclick={() => void save()}
        disabled={saving || !loaded}
      >
        {saving ? "保存中…" : "保存时段"}
      </button>
    {/if}
    {#if conflict}
      <button type="button" class="btn btn-sm btn-ghost" onclick={() => void load()} disabled={saving}>
        重新读取
      </button>
    {/if}
  </div>

  {#if error}<p class="field-error" role="alert">{error}</p>{/if}
  {#if notice}<p class="detail-note">{notice}</p>{/if}
{/if}
