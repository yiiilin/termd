import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

function manualChunk(id: string): string | undefined {
  if (id.includes("vite/preload-helper")) {
    return "vite-preload-helper";
  }
  if (id.includes("node_modules/react/") || id.includes("node_modules/react-dom/")) {
    return "react-vendor";
  }
  if (
    id.includes("node_modules/@xterm/xterm/") ||
    id.includes("node_modules/@xterm/addon-fit/") ||
    id.includes("node_modules/@xterm/addon-search/")
  ) {
    return "xterm-vendor";
  }
  if (id.includes("node_modules/lucide-react/")) {
    return "icon-vendor";
  }

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

function resolveModulePreloadDependencies(_filename: string, deps: string[], context: { hostType: "html" | "js" }): string[] {
  if (context.hostType === "js") {
    // 中文注释：动态 import 自身已经按需拉取入口 chunk；不预取依赖，避免冷路径资源进入首屏。
    return [];
  }
  return deps.filter((dep) => {
    const fileName = dep.split("/").pop() ?? dep;
    return !fileName.startsWith("monaco-") &&
      !fileName.startsWith("editor.main-") &&
      !fileName.startsWith("PairingQrScanner-") &&
      !fileName.startsWith("qr-scanner-");
  });
}

export default defineConfig({
  base: "./",
  plugins: [react()],
  build: {
    modulePreload: {
      resolveDependencies: resolveModulePreloadDependencies,
    },
    // 中文注释：Monaco runtime 已经懒加载并单独成块；4MB 以内是可预期的编辑器运行时预算。
    chunkSizeWarningLimit: 4096,
    rollupOptions: {
      output: {
        manualChunks: manualChunk,
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
