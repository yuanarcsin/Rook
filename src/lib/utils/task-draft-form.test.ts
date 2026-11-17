import { describe, expect, it } from "vitest";

import type { TaskDraftResult } from "$lib/api";
import {
  CLEAR_FEEDBACK,
  CONFLICT_MESSAGE,
  blankForm,
  draftOfForm,
  failureFeedback,
  formOfDraft,
  parseEnvText,
  parseExitCodes,
  parsePortList,
  staleFeedback,
  writeDraft,
  type DraftForm,
  type DraftWriteIo,
  type EditorFeedback,
} from "$lib/utils/task-draft-form";

/** 一份通过本地校验的表单：只改必填项，其余沿用默认值。 */
function validForm(patch: Partial<DraftForm> = {}): DraftForm {
  const base = blankForm();
  return {
    ...base,
    id: "ykt-gateway",
    name: "网关",
    command: "node",
    workingDir: "D:/scripts/ykt",
    ...patch,
  };
}

function draftResult(newRevision = "next"): TaskDraftResult {
  return {
    newRevision,
    changed: true,
  } as unknown as TaskDraftResult;
}

interface FakeIo {
  calls: string[];
  io: DraftWriteIo;
}

function fakeIo(patch: Partial<DraftWriteIo> = {}): FakeIo {
  const calls: string[] = [];
  const io: DraftWriteIo = {
    readRevision: async () => "disk",
    currentRevision: () => "disk",
    setRevision: (revision) => calls.push("revision:" + revision),
    setBusy: (busy) => calls.push("busy:" + busy),
    apply: (feedback) => calls.push("apply:" + (feedback.error ?? "none")),
    onSaved: (result) => calls.push("saved:" + result.newRevision),
    onFailure: () => calls.push("failure"),
    ...patch,
  };
  return { calls, io };
}

describe("blankForm / draftOfForm 默认值闭环", () => {
  it("默认字段与后端样板值一致", () => {
    const form = blankForm();
    expect(form.enabled).toBe(true);
    expect(form.notify).toBe(true);
    expect(form.envProxy).toBe("bypass_localhost");
    expect(form.health.kind).toBe("process");
    expect(form.health.intervalMs).toBe("10000");
    expect(form.restart.windowMs).toBe("600000");
    expect(form.restart.maxStartsInWindow).toBe("6");
    expect(form.logs.rotation).toBe("size");
    expect(form.power.resumeGraceMs).toBe("");
  });

  it("每次调用返回独立对象", () => {
    const first = blankForm();
    first.health.kind = "http";
    expect(blankForm().health.kind).toBe("process");
  });

  it("id 为空时拒绝生成草稿", () => {
    expect(() => draftOfForm(validForm({ id: "  " }))).toThrow("id 不能为空");
    expect(() => draftOfForm(blankForm())).toThrow("id 不能为空");
  });

  it("工作目录与命令不能为空", () => {
    expect(() => draftOfForm(validForm({ workingDir: " " }))).toThrow(
      "工作目录不能为空",
    );
    expect(() => draftOfForm(validForm({ command: "" }))).toThrow("命令不能为空");
  });

  it("默认表单能生成草稿并保留窗口字段", () => {
    const draft = draftOfForm(validForm());
    expect(draft.id).toBe("ykt-gateway");
    expect(draft.restart.windowMs).toBe(600000);
    expect(draft.restart.maxStartsInWindow).toBe(6);
    expect(draft.health.maxAgeMs).toBeNull();
    expect(draft.power.resumeGraceMs).toBeNull();
  });
});

describe("formOfDraft / draftOfForm roundtrip", () => {
  it("args / env / ports / 退出码 / 窗口字段往返一致", () => {
    const form = validForm({
      args: "-k\n5060",
      ports: "3000, 3001",
      env: "A=1\nB=x=y",
      panelUrl: "http://127.0.0.1:3000/",
      health: {
        ...blankForm().health,
        kind: "http",
        url: "http://127.0.0.1:3000/health",
        expectStatus: "200, 204",
        needsHumanStatus: "login_required, blocked",
      },
      restart: { ...blankForm().restart, pauseOnExitCodes: "0, 3" },
      stop: { method: "terminate", graceMs: "1200", timeoutMs: "9000" },
      logs: { ...blankForm().logs, dir: "D:/logs", encoding: "gb18030" },
      power: { resumeGraceMs: "8000" },
    });
    const draft = draftOfForm(form);
    expect(draft.args).toEqual(["-k", "5060"]);
    expect(draft.env).toEqual({ A: "1", B: "x=y" });
    expect(draft.ports).toEqual([3000, 3001]);
    expect(draft.restart.pauseOnExitCodes).toEqual([0, 3]);
    expect(formOfDraft(draft)).toEqual(form);
  });

  it("草稿里的 null 字段回填为空串", () => {
    const draft = draftOfForm(validForm());
    const form = formOfDraft(draft);
    expect(form.panelUrl).toBe("");
    expect(form.health.path).toBe("");
    expect(form.health.url).toBe("");
    expect(form.health.expectBodyContains).toBe("");
    expect(form.logs.dir).toBe("");
  });

  it("空的可选文本写成 null，空列表写成空数组", () => {
    const draft = draftOfForm(validForm({ ports: "", env: "", args: "" }));
    expect(draft.panelUrl).toBeNull();
    expect(draft.ports).toEqual([]);
    expect(draft.env).toEqual({});
    expect(draft.args).toEqual([]);
  });
});

describe("数字与范围校验", () => {
  it("非数字报字段名", () => {
    expect(() => draftOfForm(validForm({ ports: "abc" }))).toThrow(
      "ports 必须是非负整数",
    );
    expect(() =>
      draftOfForm(validForm({ restart: { ...blankForm().restart, multiplier: "x" } })),
    ).toThrow("restart.multiplier 必须是数字");
  });

  it("小于下界报字段名", () => {
    expect(() => draftOfForm(validForm({ ports: "0" }))).toThrow(
      "ports 不能小于 1",
    );
    expect(() =>
      draftOfForm(validForm({ health: { ...blankForm().health, intervalMs: "999" } })),
    ).toThrow("health.interval_ms 不能小于 1000");
  });

  it("大于上界报字段名", () => {
    expect(() =>
      draftOfForm(validForm({ stop: { ...blankForm().stop, graceMs: "60001" } })),
    ).toThrow("stop.grace_ms 不能大于 60000");
    expect(() =>
      draftOfForm(
        validForm({
          restart: { ...blankForm().restart, windowMs: "86400001" },
        }),
      ),
    ).toThrow("restart.window_ms 不能大于 86400000");
  });
});

describe("parseEnvText", () => {
  it("忽略空行并保留值里的等号", () => {
    expect(parseEnvText("\nA=1\n\nB=x=y\n")).toEqual({ A: "1", B: "x=y" });
  });

  it("缺少等号时报行号", () => {
    expect(() => parseEnvText("A=1\nBROKEN")).toThrow(
      "env 第 2 行不是 KEY=VALUE",
    );
  });

  it("以等号开头的行视为缺少变量名（当前实现不接受注释行）", () => {
    expect(() => parseEnvText("=value")).toThrow("env 第 1 行不是 KEY=VALUE");
    expect(() => parseEnvText("# comment")).toThrow("env 第 1 行不是 KEY=VALUE");
  });
});

describe("parsePortList / parseExitCodes", () => {
  it("逗号与空白都能分隔，保留重复值", () => {
    expect(parsePortList("3000,  3001")).toEqual([3000, 3001]);
    expect(parsePortList("3000,,3001,")).toEqual([3000, 3001]);
    expect(parsePortList("80, 80")).toEqual([80, 80]);
  });

  it("端口从 1 起，退出码允许 0", () => {
    expect(() => parsePortList("0")).toThrow("ports 不能小于 1");
    expect(parseExitCodes("0, 3, 7")).toEqual([0, 3, 7]);
    expect(() => parseExitCodes("-1")).toThrow(
      "restart.pause_on_exit_codes 必须是非负整数",
    );
  });
});

describe("failureFeedback / staleFeedback", () => {
  it("普通错误只带 message", () => {
    const feedback = failureFeedback(new Error("boom"));
    expect(feedback.error).toBe("boom");
    expect(feedback.detail).toBeNull();
    expect(feedback.conflict).toBe(false);
  });

  it("校验错误保留 detail，由 detail 展示", () => {
    const feedback = failureFeedback({
      stage: "validate",
      message: "bad",
      currentRevision: null,
      line: 3,
      col: null,
    });
    expect(feedback.error).toBeNull();
    expect(feedback.detail?.line).toBe(3);
  });

  it("冲突错误固定文案并标记 conflict", () => {
    const feedback = failureFeedback({
      stage: "conflict",
      message: "changed",
      currentRevision: "abc",
      line: null,
      col: null,
    });
    expect(feedback.error).toBe(CONFLICT_MESSAGE);
    expect(feedback.conflict).toBe(true);
    expect(feedback.detail?.currentRevision).toBe("abc");
  });

  it("revision 未变或为空时不报冲突", () => {
    expect(staleFeedback("", "abc")).toBeNull();
    expect(staleFeedback("abc", "abc")).toBeNull();
  });

  it("revision 变化时给出冲突反馈", () => {
    const feedback = staleFeedback("old", "new") as EditorFeedback;
    expect(feedback.conflict).toBe(true);
    expect(feedback.error).toBe(CONFLICT_MESSAGE);
    expect(feedback.detail?.stage).toBe("conflict");
    expect(feedback.detail?.currentRevision).toBe("new");
  });
});

describe("writeDraft", () => {
  it("成功时更新 revision 并交回结果", async () => {
    const { calls, io } = fakeIo();
    await writeDraft(io, async (expected) => {
      calls.push("write:" + expected);
      return draftResult("next");
    });
    expect(calls).toContain("write:disk");
    expect(calls).toContain("revision:disk");
    expect(calls).toContain("saved:next");
    expect(calls[0]).toBe("busy:true");
    expect(calls[calls.length - 1]).toBe("busy:false");
  });

  it("revision 冲突时不写入，只应用冲突反馈", async () => {
    const { calls, io } = fakeIo({ currentRevision: () => "open" });
    await writeDraft(io, async () => {
      calls.push("write");
      return draftResult();
    });
    expect(calls).not.toContain("write");
    expect(calls).toContain("apply:" + CONFLICT_MESSAGE);
    expect(calls[calls.length - 1]).toBe("busy:false");
  });

  it("写入失败时应用错误并通知组件", async () => {
    const { calls, io } = fakeIo();
    await writeDraft(io, async () => {
      throw new Error("disk full");
    });
    expect(calls).toContain("apply:disk full");
    expect(calls).toContain("failure");
    expect(calls[calls.length - 1]).toBe("busy:false");
  });

  it("开始时清空错误区", async () => {
    const { calls, io } = fakeIo({ apply: (feedback) => calls.push(feedback === CLEAR_FEEDBACK ? "clear" : "apply") });
    await writeDraft(io, async () => draftResult());
    expect(calls[0]).toBe("busy:true");
    expect(calls[1]).toBe("clear");
  });
});
