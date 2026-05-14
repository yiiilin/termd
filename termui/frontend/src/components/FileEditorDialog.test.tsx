import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FileEditorDialog, resetFileEditorDialogMonacoCacheForTests } from "./FileEditorDialog";

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

  it("saving 时保持内容可见并禁用关闭保存按钮", () => {
    render(
      <FileEditorDialog
        open
        path="/tmp/saving.txt"
        initialText="pending"
        saving
        onSave={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("File text")).toHaveValue("pending");
    expect(screen.getByRole("button", { name: "Saving" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Close editor" })).toBeDisabled();
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
});
