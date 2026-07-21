import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FileEditorDialog, resetFileEditorDialogMonacoCacheForTests } from "./FileEditorDialog";

vi.mock("./MonacoCodeEditor", async () => {
  const React = await import("react");
  return {
    default: function MockMonacoCodeEditor(props: { onUnavailable?: () => void }) {
      React.useEffect(() => {
        // 中文注释：模拟 Monaco/CSS 在组件挂载后异步失败，覆盖真实懒加载失败路径。
        const timer = window.setTimeout(() => props.onUnavailable?.(), 0);
        return () => window.clearTimeout(timer);
      }, [props.onUnavailable]);
      return <div data-testid="mock-monaco-editor" />;
    },
  };
});

describe("FileEditorDialog", () => {
  beforeEach(() => {
    // 组件级测试覆盖 textarea fallback 的行为；生产环境会加载 Monaco。
    (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__ = true;
  });

  afterEach(() => {
    resetFileEditorDialogMonacoCacheForTests();
    delete (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__;
  });

  it("关闭时不渲染对话框", () => {
    render(
      <FileEditorDialog
        open={false}
        path="/tmp/a.txt"
        initialText="hello"
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("使用 textarea 回退编辑并保存当前文本", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();

    render(
      <FileEditorDialog
        open
        path="/tmp/a.txt"
        name="a.txt"
        initialText="hello"
        onSave={onSave}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByRole("dialog", { name: "a.txt" })).toBeInTheDocument();
    expect(screen.getByText("/tmp/a.txt")).toBeInTheDocument();

    const editor = screen.getByLabelText("File text");
    expect(screen.getByLabelText("Line numbers")).toBeInTheDocument();
    expect(screen.getByLabelText("Editor minimap")).toBeInTheDocument();
    expect(screen.getByRole("dialog").querySelector(".file-editor-footer")).toHaveClass("single-row");
    await user.clear(editor);
    await user.type(editor, "hello\nworld");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSave).toHaveBeenCalledWith("hello\nworld");
    await waitFor(() => expect(screen.getByText("modified")).toBeInTheDocument());
  });

  it("loading 时展示加载状态并禁用保存", () => {
    render(
      <FileEditorDialog
        open
        path="/tmp/loading.txt"
        initialText=""
        loading
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByRole("dialog")).toContainElement(screen.getByText("/tmp/loading.txt"));
    expect(screen.getByRole("dialog").querySelector('[aria-busy="true"]')).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("saving 时保持内容可见并禁用所有关闭路径", () => {
    const onClose = vi.fn();
    render(
      <FileEditorDialog
        open
        path="/tmp/saving.txt"
        initialText="pending"
        saving
        onSave={vi.fn()}
        onClose={onClose}
      />,
    );

    expect(screen.getByLabelText("File text")).toHaveValue("pending");
    expect(screen.getByRole("button", { name: "Saving" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close editor" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    fireEvent.mouseDown(screen.getByRole("dialog").parentElement!);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("展示错误并允许用户继续编辑", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();

    render(
      <FileEditorDialog
        open
        path="/tmp/error.txt"
        initialText="old"
        error="save failed"
        onSave={onSave}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent("save failed");
    await user.clear(screen.getByLabelText("File text"));
    await user.type(screen.getByLabelText("File text"), "new");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSave).toHaveBeenCalledWith("new");
  });

  it("dirty 时所有关闭入口都只发出关闭请求并保留草稿", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();

    render(
      <FileEditorDialog
        open
        path="/tmp/draft.txt"
        initialText="saved"
        onSave={vi.fn()}
        onClose={onClose}
      />,
    );

    const editor = screen.getByLabelText("File text");
    await user.clear(editor);
    await user.type(editor, "unsaved draft");

    await user.click(screen.getByRole("button", { name: "Close editor" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.mouseDown(screen.getByRole("dialog").parentElement!);
    editor.focus();
    await user.keyboard("{Escape}");

    expect(onClose).toHaveBeenCalledTimes(4);
    expect(editor).toHaveValue("unsaved draft");
  });

  it("保存失败后保留草稿和 modified 状态", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn().mockRejectedValue(new Error("save failed"));

    render(
      <FileEditorDialog
        open
        path="/tmp/draft.txt"
        initialText="saved"
        onSave={onSave}
        onClose={vi.fn()}
      />,
    );

    const editor = screen.getByLabelText("File text");
    await user.clear(editor);
    await user.type(editor, "unsaved draft");
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledWith("unsaved draft"));
    expect(editor).toHaveValue("unsaved draft");
    expect(screen.getByText("modified")).toBeInTheDocument();
  });

  it("切换文件后同步新的初始文本", () => {
    const { rerender } = render(
      <FileEditorDialog
        open
        path="/tmp/one.txt"
        initialText="one"
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("File text")).toHaveValue("one");

    rerender(
      <FileEditorDialog
        open
        path="/tmp/two.txt"
        initialText="two"
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("File text")).toHaveValue("two");
  });

  it("Monaco 渲染失败时回退到 textarea 编辑器", async () => {
    delete (globalThis as { __TERMD_TEST_DISABLE_MONACO__?: boolean }).__TERMD_TEST_DISABLE_MONACO__;

    render(
      <FileEditorDialog
        open
        path="/tmp/fallback.txt"
        initialText="fallback"
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(await screen.findByTestId("mock-monaco-editor")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText("File text")).toHaveValue("fallback"));
    expect(screen.queryByTestId("mock-monaco-editor")).not.toBeInTheDocument();
  });
});
