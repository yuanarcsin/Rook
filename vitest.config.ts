import { defineConfig } from "vitest/config";
import { fileURLToPath } from "node:url";

// 只跑纯 TypeScript 单测：不加载 SvelteKit/Svelte 插件，避免与 `vite.config.js`
// 的 SvelteKit 插件在测试模式下互相干扰；也不引入 jsdom（当前测试无 DOM 依赖）。
export default defineConfig({
  resolve: {
    alias: {
      $lib: fileURLToPath(new URL("./src/lib", import.meta.url)),
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    // 不开 globals：测试文件显式 `import { describe, it, expect } from "vitest"`。
    globals: false,
  },
});
