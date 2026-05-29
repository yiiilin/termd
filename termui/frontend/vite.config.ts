import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

function monacoManualChunk(id: string): string | undefined {
  if (!id.includes("monaco-editor")) {
    return undefined;
  }

  // 中文注释：Monaco 只在文件编辑器打开时动态加载；语言服务拆成稳定 chunk。
  // editor/base/platform 内部互相引用较多，强拆会产生 Rollup circular chunk，所以保留为同一运行时块。
  if (id.includes("/vs/language/typescript/")) {
    return "monaco-language-typescript";
  }
  if (id.includes("/vs/language/json/")) {
    return "monaco-language-json";
  }
  if (id.includes("/vs/language/css/")) {
    return "monaco-language-css";
  }
  if (id.includes("/vs/language/html/")) {
    return "monaco-language-html";
  }
  if (id.includes("/vs/basic-languages/")) {
    return "monaco-basic-languages";
  }
  return "monaco-runtime";
}

export default defineConfig({
  base: "./",
  plugins: [react()],
  build: {
    // 中文注释：Monaco runtime 已经懒加载并单独成块；4MB 以内是可预期的编辑器运行时预算。
    chunkSizeWarningLimit: 4096,
    rollupOptions: {
      output: {
        manualChunks: monacoManualChunk,
      },
    },
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
  },
  preview: {
    host: "127.0.0.1",
    port: 4173,
  },
});
