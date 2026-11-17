// 系统设置相关的编排：开机自启、系统通知、目录、诊断导出。
//
// 统一把“读取失败”收成 { status, error }，页面只负责展示，不重复 try/catch。

import {
  exportDiagnostics,
  getAutostartStatus,
  getToastStatus,
  openAppPath,
  testToast,
  type AppPathKind,
  type AutostartStatus,
  type DiagnosticsResult,
  type ToastStatus,
} from "$lib/api";
import { errorMessage } from "$lib/utils/task-utils";

export type Loaded<T> = { status: T | null; error: string | null };

export async function loadAutostartStatus(): Promise<Loaded<AutostartStatus>> {
  try {
    return { status: await getAutostartStatus(), error: null };
  } catch (failure) {
    return { status: null, error: errorMessage(failure) };
  }
}

export async function loadToastStatus(): Promise<Loaded<ToastStatus>> {
  try {
    return { status: await getToastStatus(), error: null };
  } catch (failure) {
    return { status: null, error: errorMessage(failure) };
  }
}

export async function sendTestToast(): Promise<Loaded<ToastStatus>> {
  try {
    return { status: await testToast(), error: null };
  } catch (failure) {
    return { status: null, error: errorMessage(failure) };
  }
}

export async function openDirectory(kind: AppPathKind): Promise<void> {
  await openAppPath(kind);
}

export async function exportDiagnosticsPackage(): Promise<DiagnosticsResult> {
  return exportDiagnostics();
}
