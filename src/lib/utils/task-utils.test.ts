import { describe, expect, it } from "vitest";

import type { AllowedActions, PausedReason, TaskSnapshot, TaskStatus } from "$lib/api";
import {
  autostartLabel,
  canRun,
  countMatches,
  displayStatus,
  displayStatusLabel,
  formatBytes,
  highlightText,
  needsAttention,
  PAUSED_REASON_LABEL,
  pausedReasonLabel,
  statusTone,
  summarizeTasks,
} from "$lib/utils/task-utils";

/**
 * 构造测试用快照；只暴露本文件关心的字段，其余取默认值。
 *
 * `displayStatus` / `allowedActions` 默认**不填**，用来覆盖"旧快照"下前端的
 * 本地推导回退；需要测后端字段时显式传入。
 */
function makeTask(options: {
  valid?: boolean;
  enabled?: boolean;
  status?: TaskStatus | null;
  lastStatus?: string | null;
  pausedReason?: string | null;
  needsHuman?: boolean;
  displayStatus?: string;
  allowedActions?: AllowedActions;
} = {}): TaskSnapshot {
  const {
    valid = true,
    enabled = true,
    status = "stopped",
    lastStatus = "stopped",
    pausedReason = null,
    needsHuman = false,
    displayStatus: statusOverride,
    allowedActions,
  } = options;
  const task = {
    id: "task-1",
    name: "示例任务",
    enabled,
    command: "python",
    args: ["main.py"],
    workingDir: "C:/demo",
    autoStart: true,
    elevated: false,
    notify: true,
    healthType: "process",
    ports: [],
    panelUrl: null,
    plugin: null,
    schedule: {
      enabled: false,
      windows: [],
      inWindow: false,
      nextStartAt: null,
      nextEndAt: null,
    },
    stop: { method: "graceful", graceMs: 5000, timeoutMs: 15000 },
    valid,
    issues: [],
    runtime:
      status === null
        ? null
        : {
            status,
            pid: status === "running" ? 1234 : null,
            instanceId: status === "running" ? "instance-0001" : null,
            startedAt: null,
            exitCode: null,
            exitReason: null,
            lastError: null,
            treeEmpty: null,
            forcedKill: false,
            logPath: "C:/demo/task.log",
            logStats: {
              readBytes: 0,
              writtenBytes: 0,
              droppedBytes: 0,
              ioErrors: 0,
              readErrors: 0,
              rotations: 0,
              queueDrops: 0,
            },
          },
    supervisor: {
      desiredOverride: null,
      consecutiveFailures: 0,
      totalStarts: 0,
      totalRestarts: 0,
      pausedReason,
      lastStatus,
      nextRetryAt: null,
      needsHuman: {
        active: needsHuman,
        source: needsHuman ? "exit_code" : null,
        message: null,
        since: null,
      },
      restartPending: false,
    },
  };
  return {
    ...task,
    ...(statusOverride === undefined ? {} : { displayStatus: statusOverride }),
    ...(allowedActions === undefined ? {} : { allowedActions }),
  } as TaskSnapshot;
}

describe("canRun", () => {
  it("valid + enabled + stopped 时可启动与重启，不可停止", () => {
    const task = makeTask({ status: "stopped" });
    expect(canRun(task, "start")).toBe(true);
    expect(canRun(task, "restart")).toBe(true);
    expect(canRun(task, "stop")).toBe(false);
  });

  it("running 时可停止与重启，不可再启动", () => {
    const task = makeTask({ status: "running" });
    expect(canRun(task, "start")).toBe(false);
    expect(canRun(task, "stop")).toBe(true);
    expect(canRun(task, "restart")).toBe(true);
  });

  it("starting 时三个动作都不可用", () => {
    const task = makeTask({ status: "starting" });
    expect(canRun(task, "start")).toBe(false);
    expect(canRun(task, "stop")).toBe(false);
    expect(canRun(task, "restart")).toBe(false);
  });

  it("stopping 时只允许停止（收敛实例）", () => {
    const task = makeTask({ status: "stopping" });
    expect(canRun(task, "stop")).toBe(true);
    expect(canRun(task, "start")).toBe(false);
    expect(canRun(task, "restart")).toBe(false);
  });

  it("disabled 时不可启动/重启，但仍可停止", () => {
    const stopped = makeTask({ status: "stopped", enabled: false });
    expect(canRun(stopped, "start")).toBe(false);
    expect(canRun(stopped, "restart")).toBe(false);
    expect(canRun(stopped, "stop")).toBe(false);

    const running = makeTask({ status: "running", enabled: false });
    expect(canRun(running, "stop")).toBe(true);
    expect(canRun(running, "restart")).toBe(false);
  });

  it("invalid 或没有 runtime 时任何动作都不可用", () => {
    const invalid = makeTask({ status: "stopped", valid: false });
    expect(canRun(invalid, "start")).toBe(false);
    expect(canRun(invalid, "stop")).toBe(false);
    expect(canRun(invalid, "restart")).toBe(false);

    const noRuntime = makeTask({ status: null });
    expect(canRun(noRuntime, "start")).toBe(false);
    expect(canRun(noRuntime, "stop")).toBe(false);
    expect(canRun(noRuntime, "restart")).toBe(false);
  });

  it("存在 allowedActions 时以后端字段为准，即使与本地推导冲突", () => {
    // 后端说全禁用：即使 status=stopped 且 enabled，也不得自行放行。
    const denied = makeTask({
      status: "stopped",
      allowedActions: { start: false, stop: false, restart: false },
    });
    expect(canRun(denied, "start")).toBe(false);
    expect(canRun(denied, "restart")).toBe(false);
    expect(canRun(denied, "stop")).toBe(false);

    // 后端放行：即使本地推导会拒绝（stopped 不可 stop），也照用后端值。
    const allowed = makeTask({
      status: "stopped",
      allowedActions: { start: true, stop: true, restart: true },
    });
    expect(canRun(allowed, "start")).toBe(true);
    expect(canRun(allowed, "stop")).toBe(true);
  });

  it("字段缺失（旧快照）时回退本地推导", () => {
    expect(canRun(makeTask({ status: "stopped" }), "start")).toBe(true);
    expect(canRun(makeTask({ status: "running" }), "stop")).toBe(true);
  });
});

describe("displayStatus", () => {  it("runtime running 但 supervisor 仍 starting 时显示启动中", () => {
    const task = makeTask({ status: "running", lastStatus: "starting" });
    expect(displayStatus(task)).toBe("starting");
  });

  it("runtime running 且 supervisor 已就绪时显示运行中", () => {
    expect(displayStatus(makeTask({ status: "running", lastStatus: "healthy" }))).toBe(
      "running",
    );
  });

  it("starting / stopping 直接透传", () => {
    expect(displayStatus(makeTask({ status: "starting" }))).toBe("starting");
    expect(displayStatus(makeTask({ status: "stopping" }))).toBe("stopping");
  });

  it("停止后优先显示 needs_human", () => {
    const task = makeTask({
      status: "stopped",
      lastStatus: "backoff",
      pausedReason: "crash_loop",
      needsHuman: true,
    });
    expect(displayStatus(task)).toBe("needs_human");
  });

  it("停止后显示 paused", () => {
    const task = makeTask({
      status: "stopped",
      lastStatus: "backoff",
      pausedReason: "crash_loop",
    });
    expect(displayStatus(task)).toBe("paused");
  });

  it("停止后回到 backoff", () => {
    const task = makeTask({ status: "stopped", lastStatus: "backoff" });
    expect(displayStatus(task)).toBe("backoff");
  });

  it("普通停止显示 stopped，缺失 runtime 时回落到 unknown", () => {
    expect(displayStatus(makeTask({ status: "stopped" }))).toBe("stopped");
    expect(displayStatus(makeTask({ status: null }))).toBe("unknown");
  });

  it("存在 displayStatus 时优先用后端值", () => {
    // 后端已判定需要人工处理：即使 runtime 看似 running 也不自行覆盖。
    expect(
      displayStatus(
        makeTask({ status: "running", lastStatus: "healthy", displayStatus: "needs_human" }),
      ),
    ).toBe("needs_human");
    expect(displayStatus(makeTask({ status: null, displayStatus: "disabled" }))).toBe(
      "disabled",
    );
    expect(displayStatus(makeTask({ status: "stopped", displayStatus: "invalid" }))).toBe(
      "invalid",
    );
  });
});

describe("displayStatusLabel", () => {
  it("后端全部展示状态都有中文文案", () => {
    expect(displayStatusLabel.invalid).toBe("无效");
    expect(displayStatusLabel.unknown).toBe("—");
    expect(displayStatusLabel.starting).toBe("启动中");
    expect(displayStatusLabel.running).toBe("运行中");
    expect(displayStatusLabel.stopping).toBe("停止中");
    expect(displayStatusLabel.stopped).toBe("已停止");
    expect(displayStatusLabel.paused).toBe("已暂停");
    expect(displayStatusLabel.needs_human).toBe("需要人工处理");
    expect(displayStatusLabel.backoff).toBe("退避重试");
    expect(displayStatusLabel.disabled).toBe("已禁用");
  });
});

describe("statusTone", () => {
  it("已定义状态使用固定色调", () => {
    expect(statusTone("running")).toBe("ok");
    expect(statusTone("starting")).toBe("info");
    expect(statusTone("stopping")).toBe("warn");
    expect(statusTone("backoff")).toBe("warn");
    expect(statusTone("needs_human")).toBe("warn");
    expect(statusTone("paused")).toBe("bad");
  });

  it("未知状态与停止使用 muted", () => {
    expect(statusTone("stopped")).toBe("muted");
    expect(statusTone("unknown")).toBe("muted");
    expect(statusTone("whatever")).toBe("muted");
  });
});

describe("needsAttention", () => {
  it("配置无效、needs_human、paused 都算需要关注", () => {
    expect(needsAttention(makeTask({ valid: false }))).toBe(true);
    expect(needsAttention(makeTask({ needsHuman: true }))).toBe(true);
    expect(needsAttention(makeTask({ pausedReason: "crash_loop" }))).toBe(true);
  });

  it("正常任务不需要关注", () => {
    expect(needsAttention(makeTask({ status: "running" }))).toBe(false);
    expect(needsAttention(makeTask({ status: "stopped" }))).toBe(false);
  });
});

describe("pausedReasonLabel", () => {
  it("真实 pausedReason 取值都有中文标签", () => {
    expect(pausedReasonLabel("crash_loop")).toBe("连续崩溃");
    expect(pausedReasonLabel("crash_no_restart")).toBe("退出后不重启");
    expect(pausedReasonLabel("restart_budget")).toBe("重启过于频繁");
    expect(pausedReasonLabel("needs_human")).toBe("需要人工处理");
    expect(pausedReasonLabel("user_pause")).toBe("手动暂停");
    expect(pausedReasonLabel("elevation_unavailable")).toBe("提权不可用");
  });

  it("未知取值原样返回，便于排障", () => {
    // 故意使用与后端无关的占位值，避免与后续新增的 pausedReason 取值耦合。
    expect(pausedReasonLabel("some_unknown_reason")).toBe("some_unknown_reason");
    expect(pausedReasonLabel("")).toBe("");
  });

  it("union 的每个取值都有标签，且标签表没有遗漏", () => {
    // 穷尽性由 PAUSED_REASON_LABEL 的 `satisfies Record<PausedReason, string>` 保证；
    // 这里的数组再覆盖一次运行时行为，漏写标签会得到原值而失败。
    const reasons: PausedReason[] = [
      "crash_loop",
      "restart_budget",
      "needs_human",
      "crash_no_restart",
      "user_pause",
      "elevation_unavailable",
    ];
    for (const reason of reasons) {
      const label = pausedReasonLabel(reason);
      expect(label.length).toBeGreaterThan(0);
      expect(label).not.toBe(reason);
    }
    expect(Object.keys(PAUSED_REASON_LABEL).length).toBeGreaterThanOrEqual(6);
  });

  it("保留旧 state.json 的历史取值映射", () => {
    expect(pausedReasonLabel("user")).toBe("手动暂停");
    expect(pausedReasonLabel("port_conflict")).toBe("端口冲突");
    expect(pausedReasonLabel("elevation")).toBe("提权不可用");
  });
});

describe("summarizeTasks", () => {
  it("统计总数 / 运行中 / 需关注 / 已停止", () => {
    const tasks = [
      makeTask({ status: "running", lastStatus: "healthy" }),
      makeTask({ status: "starting" }),
      makeTask({ status: "stopped" }),
      makeTask({ status: "stopped", pausedReason: "crash_loop" }),
      makeTask({ status: "stopped", valid: false }),
    ];
    expect(summarizeTasks(tasks)).toEqual({
      total: 5,
      running: 2,
      attention: 2,
      stopped: 1,
    });
  });

  it("stopping 计入运行中，空列表返回零", () => {
    expect(summarizeTasks([makeTask({ status: "stopping" })])).toEqual({
      total: 1,
      running: 1,
      attention: 0,
      stopped: 0,
    });
    expect(summarizeTasks([])).toEqual({
      total: 0,
      running: 0,
      attention: 0,
      stopped: 0,
    });
  });
});

describe("formatBytes", () => {
  it("B / KB / MB 三档并保留一位小数", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(1048576)).toBe("1.0 MB");
    expect(formatBytes(5 * 1048576 + 524288)).toBe("5.5 MB");
  });
});

describe("autostartLabel", () => {
  const base = {
    enabled: true,
    mode: "run_key" as const,
    expectedValue: "campus.exe",
    actualValue: "campus.exe",
    matches: true,
    registered: true,
    error: null as string | null,
  };

  it("读取失败优先于其他状态", () => {
    expect(autostartLabel({ ...base, error: "拒绝访问", registered: false })).toBe(
      "读取失败",
    );
  });

  it("未注册 / 已注册 / 路径不匹配", () => {
    expect(autostartLabel({ ...base, registered: false, matches: false })).toBe(
      "未注册",
    );
    expect(autostartLabel(base)).toBe("已注册");
    expect(autostartLabel({ ...base, matches: false })).toBe("路径不匹配");
  });
});

describe("highlightText", () => {
  it("空查询返回整行未命中片段", () => {
    expect(highlightText("hello", "   ")).toEqual([{ text: "hello", hit: false }]);
  });

  it("多处命中按顺序切分，大小写不敏感", () => {
    expect(highlightText("Error: error 42", "ERROR")).toEqual([
      { text: "Error", hit: true },
      { text: ": ", hit: false },
      { text: "error", hit: true },
      { text: " 42", hit: false },
    ]);
  });

  it("行首/行尾命中不产生空片段，未命中返回原行", () => {
    expect(highlightText("abcabc", "abc")).toEqual([
      { text: "abc", hit: true },
      { text: "abc", hit: true },
    ]);
    expect(highlightText("abc", "zzz")).toEqual([{ text: "abc", hit: false }]);
  });
});

describe("countMatches", () => {
  it("统计跨行命中次数，大小写不敏感", () => {
    expect(countMatches(["error 1", "ERROR 2", "no hit"], "error")).toBe(2);
  });

  it("同一行多次命中分别计数（不重叠），空查询为 0", () => {
    expect(countMatches(["a aa aaa"], "aa")).toBe(2);
    expect(countMatches(["aaaa"], "aa")).toBe(2);
    expect(countMatches(["anything"], "  ")).toBe(0);
    expect(countMatches([], "x")).toBe(0);
  });
});
