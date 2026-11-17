#!/usr/bin/env node
/**
 * 轻量类型契约检查（docs/19 B2）。
 *
 * 只比对 4 个核心 DTO 的**字段名与可选性**：
 *   Snapshot / TaskSnapshot / TaskDraft / PluginPreview
 *
 * Rust 侧按 `pub field: Type,` 逐行提取（这些结构体没有跨行字段）；
 * TS 侧按 `export interface X {` 到顶格 `}` 之间的 `field: type;` 提取。
 *
 * 字段名统一转 camelCase 后比较；Rust `Option<T>` 对应 TS 类型里必须出现
 * `null`（或 `?`），反之亦然。发现差异时打印并 exit 1。
 *
 * 这是文本级检查，不替代编译器：目的是在 CI 里尽早发现“后端加了字段、
 * 前端 api.ts 忘了加”这类漂移。
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const TS_FILE = "src/lib/api.ts";

/** 允许的已知差异：`Type.field` → 原因。当前为空，优先修类型而不是加白名单。 */
const ALLOW = new Map([]);

const TYPES = [
  { name: "Snapshot", rust: "src-tauri/src/snapshot.rs" },
  { name: "TaskSnapshot", rust: "src-tauri/src/snapshot.rs" },
  { name: "TaskDraft", rust: "src-tauri/src/config/task_draft.rs" },
  { name: "PluginPreview", rust: "src-tauri/src/config/plugin.rs" },
];

function camelCase(name) {
  return name.replace(/_([a-z0-9])/g, (_, ch) => ch.toUpperCase());
}

function read(relative) {
  return readFileSync(join(ROOT, relative), "utf8");
}

/** 提取 Rust `pub struct Name { ... }` 的顶层 pub 字段。 */
function rustFields(source, name) {
  const marker = `pub struct ${name} {`;
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`Rust 结构体未找到：${name}`);
  const open = start + marker.length - 1;
  const fields = new Map();
  let depth = 0;
  let lineStart = open + 1;
  for (let index = open; index < source.length; index += 1) {
    const char = source[index];
    if (char === "{") depth += 1;
    if (char === "}") {
      depth -= 1;
      if (depth === 0) break;
    }
    if (char === "\n") {
      const line = source.slice(lineStart, index);
      lineStart = index + 1;
      const match = line.match(/^\s*pub\s+([a-z0-9_]+)\s*:\s*(.+?),\s*$/);
      if (match) {
        fields.set(camelCase(match[1]), {
          optional: match[2].trim().startsWith("Option<"),
        });
      }
    }
  }
  return fields;
}

/** 提取 TS `export interface Name { ... }` 的顶层字段（顶格 `}` 结束）。 */
function tsFields(source, name) {
  const marker = `export interface ${name} {`;
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`TS 接口未找到：${name}`);
  const lines = source.slice(start + marker.length).split("\n");
  const fields = new Map();
  for (const line of lines) {
    if (line === "}") break;
    const match = line.match(/^\s{2}([A-Za-z0-9_]+)(\?)?\s*:\s*(.+?);\s*$/);
    if (!match) continue;
    fields.set(match[1], {
      optional: match[2] === "?" || /\bnull\b/.test(match[3]),
    });
  }
  return fields;
}

const tsSource = read(TS_FILE);
const problems = [];

for (const type of TYPES) {
  const rust = rustFields(read(type.rust), type.name);
  const ts = tsFields(tsSource, type.name);

  for (const [field, info] of rust) {
    if (ALLOW.has(`${type.name}.${field}`)) continue;
    const counterpart = ts.get(field);
    if (!counterpart) {
      problems.push(`${type.name}.${field}：Rust 有、api.ts 缺`);
      continue;
    }
    if (info.optional !== counterpart.optional) {
      problems.push(
        `${type.name}.${field}：可选性不一致（Rust Option=${info.optional}，TS null/?=${counterpart.optional}）`,
      );
    }
  }
  for (const field of ts.keys()) {
    if (!rust.has(field) && !ALLOW.has(`${type.name}.${field}`)) {
      problems.push(`${type.name}.${field}：api.ts 有、Rust 缺`);
    }
  }
}

if (problems.length > 0) {
  console.error("类型契约不一致：");
  for (const problem of problems) console.error(`  - ${problem}`);
  console.error(
    "\n修复方向：以后端 DTO 为准更新 src/lib/api.ts；确属有意差异时在脚本顶部 ALLOW 注明原因。",
  );
  process.exit(1);
}

console.log(`types: ${TYPES.length} structs, ok`);
