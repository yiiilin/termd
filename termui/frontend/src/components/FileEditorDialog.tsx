import { useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { Check, FileText, Loader2, X } from "lucide-react";
import type { MonacoCodeEditor } from "./MonacoCodeEditor";
import { useI18n } from "../i18n";
import type { EffectiveTheme } from "../protocol/types";
import { monacoTheme } from "../theme";

export interface FileEditorDialogProps {
  open: boolean;
  path: string;
  name?: string;
  initialText: string;
  loading?: boolean;
  saving?: boolean;
  error?: string;
  language?: string;
  theme?: EffectiveTheme;
  readOnly?: boolean;
  onSave: (text: string) => void | Promise<void>;
  onClose: () => void;
}

let monacoEditorPromise: Promise<MonacoCodeEditor | null> | null = null;

async function loadOptionalMonacoEditor(): Promise<MonacoCodeEditor | null> {
  if ((globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__) {
    return null;
  }
  if (!monacoEditorPromise) {
    monacoEditorPromise = (async () => {
      try {
        // Monaco 是主编辑体验；动态加载本地 bundle，避免首屏终端工作台承担编辑器体积。
        const module = (await import("./MonacoCodeEditor")) as {
          default?: MonacoCodeEditor;
        };
        return module.default ?? null;
      } catch {
        return null;
      }
    })();
  }
  return monacoEditorPromise;
}

// 测试辅助：允许单测隔离可选编辑器加载缓存，避免不同用例互相影响。
export function resetFileEditorDialogMonacoCacheForTests(): void {
  monacoEditorPromise = null;
}

export function FileEditorDialog({
  open,
  path,
  name,
  initialText,
  loading = false,
  saving = false,
  error,
  language,
  theme = "dark",
  readOnly = false,
  onSave,
  onClose,
}: FileEditorDialogProps) {
  const [text, setText] = useState(initialText);
  const [MonacoEditor, setMonacoEditor] = useState<MonacoCodeEditor | null>(null);
  const [monacoChecked, setMonacoChecked] = useState(false);
  const [monacoUnavailable, setMonacoUnavailable] = useState(false);
  const { t } = useI18n();

  useEffect(() => {
    if (open) {
      setText(initialText);
    }
  }, [initialText, open, path]);

  useEffect(() => {
    let active = true;
    if (!open) {
      return () => {
        active = false;
      };
    }

    setMonacoChecked(false);
    setMonacoUnavailable(false);
    void loadOptionalMonacoEditor().then((editor) => {
      if (active) {
        setMonacoEditor(() => editor);
        setMonacoChecked(true);
      }
    });

    return () => {
      active = false;
    };
  }, [open]);

  const title = name?.trim() || basename(path) || t("editor.untitled");
  const disabled = loading || saving;
  const canEdit = !disabled && !readOnly;
  const canClose = !saving;
  const isDirty = text !== initialText;
  const saveLabel = saving ? t("editor.savingButton") : t("editor.save");
  const statusText = useMemo(() => {
    if (loading) {
      return t("editor.loading");
    }
    if (saving) {
      return t("editor.saving");
    }
    if (readOnly) {
      return t("editor.readOnly");
    }
    return isDirty ? t("editor.modified") : t("editor.saved");
  }, [isDirty, loading, readOnly, saving, t]);

  if (!open) {
    return null;
  }

  return (
    <div
      className="file-editor-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (canClose && event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <section
        className="file-editor-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="file-editor-title"
        aria-describedby={error ? "file-editor-error" : undefined}
      >
        <header className="file-editor-header">
          <div className="file-editor-title-group">
            <FileText size={17} aria-hidden="true" />
            <div className="file-editor-title-text">
              <h2 id="file-editor-title">
                {title}
              </h2>
              <span title={path}>
                {path}
              </span>
            </div>
          </div>
          <div className="file-editor-actions">
            <span className="file-editor-status" aria-live="polite">
              {statusText}
            </span>
            <button type="button" className="icon-button" aria-label={t("editor.close")} disabled={!canClose} onClick={onClose}>
              <X size={16} aria-hidden="true" />
            </button>
          </div>
        </header>

        {error ? (
          <div id="file-editor-error" className="file-editor-error" role="alert">
            {error}
          </div>
        ) : null}

        <div className="file-editor-shell" aria-busy={loading || saving}>
          {loading ? (
            <div className="file-editor-loading">
              <Loader2 size={16} aria-hidden="true" />
              {t("editor.loading")}
            </div>
          ) : MonacoEditor && !monacoUnavailable ? (
            <MonacoEditor
              className="file-editor-monaco"
              height="100%"
              value={text}
              language={language}
              theme={monacoTheme(theme)}
              loading={<div className="file-editor-loading">{t("editor.loadingEditor")}</div>}
              options={{
                readOnly: !canEdit,
                lineNumbers: "on",
                glyphMargin: false,
                folding: true,
                minimap: { enabled: true, side: "right", showSlider: "mouseover" },
                scrollBeyondLastLine: false,
                wordWrap: "on",
                automaticLayout: true,
              }}
              onChange={(value: string | undefined) => {
                if (canEdit) {
                  setText(value ?? "");
                }
              }}
              onUnavailable={() => setMonacoUnavailable(true)}
            />
          ) : (
            <FallbackCodeEditor
              text={text}
              readOnly={!canEdit}
              placeholder={monacoChecked ? "" : t("editor.loadingEditor")}
              onChange={setText}
              lineNumbersLabel={t("editor.lineNumbers")}
              fileTextLabel={t("editor.fileText")}
              minimapLabel={t("editor.minimap")}
            />
          )}
        </div>

        <footer className="file-editor-footer single-row">
          <button type="button" disabled={!canClose} onClick={onClose}>
            {t("editor.cancel")}
          </button>
          <button
            type="button"
            disabled={disabled || readOnly}
            className="file-editor-save"
            onClick={() => void onSave(text)}
          >
            {saving ? <Loader2 size={15} aria-hidden="true" /> : <Check size={15} aria-hidden="true" />}
            {saveLabel}
          </button>
        </footer>
      </section>
    </div>
  );
}

function FallbackCodeEditor(props: {
  text: string;
  readOnly: boolean;
  placeholder: string;
  lineNumbersLabel: string;
  fileTextLabel: string;
  minimapLabel: string;
  onChange: (text: string) => void;
}) {
  const lines = props.text.split("\n");
  const lineNumbers = lines.map((_, index) => index + 1);

  return (
    <div className="file-editor-fallback">
      <div className="file-editor-line-numbers" aria-label={props.lineNumbersLabel}>
        {lineNumbers.map((line) => (
          <span key={line}>{line}</span>
        ))}
      </div>
      <textarea
        aria-label={props.fileTextLabel}
        value={props.text}
        readOnly={props.readOnly}
        spellCheck={false}
        placeholder={props.placeholder}
        onChange={(event) => props.onChange(event.currentTarget.value)}
      />
      <div className="file-editor-minimap" aria-label={props.minimapLabel}>
        {lines.map((line, index) => (
          <span key={`${index}:${line.slice(0, 12)}`} style={{ width: `${minimapLineWidth(line)}%` }} />
        ))}
      </div>
    </div>
  );
}

function minimapLineWidth(line: string): number {
  const trimmedLength = line.trimEnd().length;
  if (trimmedLength <= 0) {
    return 12;
  }
  return Math.max(18, Math.min(100, trimmedLength * 4));
}

function basename(path: string): string {
  const trimmed = path.trim().replace(/\/+$/, "");
  if (!trimmed || trimmed === "/") {
    return "";
  }
  return trimmed.slice(trimmed.lastIndexOf("/") + 1);
}
