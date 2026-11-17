// 任务编辑器控制器：表单状态、加载、保存、删除、冲突反馈。
//
// 纯转换/校验在 utils/task-draft-form.ts（可单测）；这里只负责把它们接到
// API、app store 与组件生命周期上，让 Svelte 文件只剩模板与少量事件。

import {
  deleteTask,
  getTaskDefaults,
  getTaskDraft,
  readConfigText,
  saveTaskDraft,
  type TaskDraft,
  type TaskDraftError,
  type TaskDraftResult,
} from "$lib/api";
import { currentConfigRevision } from "$lib/services/config";
import { app } from "$lib/state/app.svelte";
import { errorMessage } from "$lib/utils/task-utils";
import {
  CLEAR_FEEDBACK,
  blankForm,
  draftOfForm,
  failureFeedback,
  formOfDraft,
  writeDraft,
  type DraftForm,
  type DraftWriteIo,
  type EditorFeedback,
} from "$lib/utils/task-draft-form";

export class TaskEditorState {
  readonly mode = app.editor.mode;
  readonly taskId = app.editor.taskId;

  loading = $state(true);
  saving = $state(false);
  deleting = $state(false);
  revision = $state("");
  form = $state<DraftForm>(blankForm());
  error = $state<string | null>(null);
  detail = $state<TaskDraftError | null>(null);
  conflict = $state(false);
  /** 打开表单时的初始快照；与当前表单不一致即视为有未保存修改。 */
  baseline = $state(JSON.stringify(blankForm()));

  get dirty(): boolean {
    return JSON.stringify(this.form) !== this.baseline;
  }

  /** 当前配置 revision：优先快照（B6），结构错误/旧快照时回退读文本。 */
  private async currentRevision(): Promise<string> {
    const fromSnapshot = app.snapshot?.configRevision ?? "";
    return fromSnapshot.length > 0 ? fromSnapshot : (await readConfigText()).revision;
  }

  applyFeedback(patch: EditorFeedback): void {
    this.error = patch.error;
    this.detail = patch.detail;
    this.conflict = patch.conflict;
  }

  handleFailure(failure: unknown): void {
    this.applyFeedback(failureFeedback(failure));
  }

  /** 读取配置 revision；编辑模式下同时载入草稿（冲突重载也走这里）。 */
  async load(): Promise<void> {
    this.loading = true;
    this.applyFeedback(CLEAR_FEEDBACK);
    try {
      const [revision, draft] = await Promise.all([
        this.currentRevision(),
        this.mode === "edit"
          ? getTaskDraft(this.taskId)
          : getTaskDefaults().catch(() => null),
      ]);
      this.revision = revision;
      if (draft !== null) {
        this.form = formOfDraft(draft);
      }
    } catch (failure) {
      this.handleFailure(failure);
    } finally {
      this.baseline = JSON.stringify(this.form);
      this.loading = false;
    }
  }

  /** 冲突后的“重新加载”：编辑模式重读草稿，新建模式只刷新 revision。 */
  async reload(): Promise<void> {
    if (this.loading || this.saving || this.deleting) {
      return;
    }
    if (this.mode === "edit") {
      await this.load();
      return;
    }
    this.loading = true;
    this.applyFeedback(CLEAR_FEEDBACK);
    try {
      this.revision = await this.currentRevision();
    } finally {
      this.loading = false;
    }
  }

  /** 组件的 $state 适配；冲突检测与写入顺序见 writeDraft。 */
  private writeIo(busy: "saving" | "deleting"): DraftWriteIo {
    return {
      readRevision: () => this.currentRevision(),
      currentRevision: () => this.revision,
      setRevision: (value) => (this.revision = value),
      setBusy: (value) => {
        if (busy === "saving") {
          this.saving = value;
        } else {
          this.deleting = value;
        }
      },
      apply: (feedback) => this.applyFeedback(feedback),
      onSaved: (result: TaskDraftResult) => {
        app.applySnapshot(result.snapshot);
        app.closeEditor();
      },
      onFailure: () => {},
    };
  }

  async save(): Promise<void> {
    if (this.loading || this.saving || this.deleting) {
      return;
    }
    let draft: TaskDraft;
    try {
      draft = draftOfForm(this.form);
    } catch (failure) {
      this.detail = null;
      this.error = errorMessage(failure);
      return;
    }
    await writeDraft(this.writeIo("saving"), (expected) =>
      saveTaskDraft(draft, expected),
    );
  }

  async remove(): Promise<void> {
    if (this.mode !== "edit" || this.loading || this.saving || this.deleting) {
      return;
    }
    await writeDraft(this.writeIo("deleting"), (expected) =>
      deleteTask(this.taskId, expected),
    );
  }
}
