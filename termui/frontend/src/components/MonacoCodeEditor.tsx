import { useEffect, useRef, type ReactNode } from "react";
import * as monaco from "monaco-editor/esm/vs/editor/edcore.main.js";
import monacoEditorCssUrl from "monaco-editor/min/vs/editor/editor.main.css?url";
import "monaco-editor/esm/vs/basic-languages/markdown/markdown.contribution";
import "monaco-editor/esm/vs/basic-languages/python/python.contribution";
import "monaco-editor/esm/vs/basic-languages/rust/rust.contribution";
import "monaco-editor/esm/vs/basic-languages/shell/shell.contribution";
import "monaco-editor/esm/vs/basic-languages/yaml/yaml.contribution";
import "monaco-editor/esm/vs/language/css/monaco.contribution";
import "monaco-editor/esm/vs/language/html/monaco.contribution";
import "monaco-editor/esm/vs/language/json/monaco.contribution";
import "monaco-editor/esm/vs/language/typescript/monaco.contribution";
import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import cssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import tsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

export interface MonacoCodeEditorProps {
  value?: string;
  language?: string;
  theme?: string;
  height?: string | number;
  className?: string;
  loading?: ReactNode;
  options?: monaco.editor.IStandaloneEditorConstructionOptions;
  onChange?: (value?: string) => void;
  onUnavailable?: () => void;
}

type WorkerFactory = new () => Worker;
let monacoEditorCssPromise: Promise<void> | null = null;

const workerByLabel = new Map<string, WorkerFactory>([
  ["json", jsonWorker],
  ["css", cssWorker],
  ["scss", cssWorker],
  ["less", cssWorker],
  ["html", htmlWorker],
  ["handlebars", htmlWorker],
  ["razor", htmlWorker],
  ["typescript", tsWorker],
  ["javascript", tsWorker],
]);

// termd 常在内网或本机离线使用；这里直接绑定本地 Vite worker，避免 Monaco 默认走 CDN loader。
(globalThis as { MonacoEnvironment?: { getWorker?: (workerId: string, label: string) => Worker } }).MonacoEnvironment = {
  getWorker: (_workerId: string, label: string) => {
    const WorkerCtor = workerByLabel.get(label) ?? editorWorker;
    return new WorkerCtor();
  },
};

function ensureMonacoEditorCss(): Promise<void> {
  if (typeof document === "undefined") {
    return Promise.resolve();
  }
  if (monacoEditorCssPromise) {
    return monacoEditorCssPromise;
  }

  monacoEditorCssPromise = new Promise<void>((resolve, reject) => {
    const existing = document.querySelector<HTMLLinkElement>(`link[data-termd-monaco-css="${monacoEditorCssUrl}"]`);
    if (existing) {
      if (existing.dataset.termdMonacoCssStatus === "ready") {
        resolve();
        return;
      }
      if (existing.dataset.termdMonacoCssStatus === "error") {
        existing.remove();
      } else {
        existing.addEventListener("load", () => resolve(), { once: true });
        existing.addEventListener("error", () => reject(new Error("failed to load Monaco editor CSS")), { once: true });
        return;
      }
    }

    // 中文注释：Monaco 是文件编辑器冷路径；CSS 也按需注入，避免 Vite 把 300KB 样式放进首屏 HTML。
    const link = document.createElement("link");
    link.rel = "stylesheet";
    link.href = monacoEditorCssUrl;
    link.dataset.termdMonacoCss = monacoEditorCssUrl;
    link.onload = () => {
      link.dataset.termdMonacoCssStatus = "ready";
      resolve();
    };
    link.onerror = () => {
      link.dataset.termdMonacoCssStatus = "error";
      link.remove();
      reject(new Error("failed to load Monaco editor CSS"));
    };
    document.head.append(link);
  }).catch((caught) => {
    monacoEditorCssPromise = null;
    throw caught;
  });

  return monacoEditorCssPromise;
}

export type MonacoCodeEditor = typeof MonacoCodeEditor;

export default function MonacoCodeEditor({
  value = "",
  language,
  theme = "vs-dark",
  height = "100%",
  className,
  options,
  onChange,
  onUnavailable,
}: MonacoCodeEditorProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  const changeListenerRef = useRef<monaco.IDisposable | undefined>(undefined);
  const onChangeRef = useRef(onChange);
  const suppressChangeRef = useRef(false);

  useEffect(() => {
    onChangeRef.current = onChange;
  }, [onChange]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container || editorRef.current) {
      return undefined;
    }

    let cancelled = false;
    let editor: monaco.editor.IStandaloneCodeEditor | undefined;
    void ensureMonacoEditorCss().then(() => {
      if (cancelled || !container.isConnected || editorRef.current) {
        return;
      }
      editor = monaco.editor.create(container, {
        value,
        language,
        theme,
        automaticLayout: true,
        ...options,
      });
      editorRef.current = editor;
      changeListenerRef.current = editor.onDidChangeModelContent(() => {
        if (!suppressChangeRef.current) {
          onChangeRef.current?.(editor?.getValue());
        }
      });
    }).catch(() => {
      // 中文注释：样式加载失败时让父层回退到 textarea；编辑器冷路径失败不应打断整个工作台。
      if (!cancelled) {
        onUnavailable?.();
      }
    });

    return () => {
      cancelled = true;
      changeListenerRef.current?.dispose();
      changeListenerRef.current = undefined;
      editor?.getModel()?.dispose();
      editor?.dispose();
      if (editorRef.current === editor) {
        editorRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor || value === editor.getValue()) {
      return;
    }

    suppressChangeRef.current = true;
    editor.setValue(value);
    suppressChangeRef.current = false;
  }, [value]);

  useEffect(() => {
    const model = editorRef.current?.getModel();
    if (model && language) {
      monaco.editor.setModelLanguage(model, language);
    }
  }, [language]);

  useEffect(() => {
    monaco.editor.setTheme(theme);
  }, [theme]);

  useEffect(() => {
    if (options) {
      editorRef.current?.updateOptions(options);
    }
  }, [options]);

  return (
    <div className={className} style={{ height }}>
      <div ref={containerRef} className="file-editor-monaco-host" />
    </div>
  );
}
