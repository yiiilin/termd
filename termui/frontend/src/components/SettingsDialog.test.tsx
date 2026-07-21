import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { I18nProvider } from "../i18n";
import type { BrowserPreferences } from "../protocol/types";
import { SettingsDialog } from "./SettingsDialog";

const initialPreferences: BrowserPreferences = {
  language: "auto",
  theme: "system",
  notifications: "off",
  mobileShortcuts: [],
};

function SettingsHarness({
  initial = initialPreferences,
  onPreferencesChange,
}: {
  initial?: BrowserPreferences;
  onPreferencesChange?: (preferences: BrowserPreferences) => void;
}) {
  const [open, setOpen] = useState(false);
  const [preferences, setPreferences] = useState(initial);

  const handlePreferencesChange = (nextPreferences: BrowserPreferences) => {
    onPreferencesChange?.(nextPreferences);
    setPreferences(nextPreferences);
  };

  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        Open settings
      </button>
      <SettingsDialog
        open={open}
        preferences={preferences}
        effectiveLocale="en-US"
        effectiveTheme="light"
        onPreferencesChange={handlePreferencesChange}
        onClose={() => setOpen(false)}
      />
    </>
  );
}

describe("SettingsDialog", () => {
  it("keeps keyboard focus inside and restores it after Escape closes the dialog", async () => {
    const user = userEvent.setup();
    render(<SettingsHarness />);

    const trigger = screen.getByRole("button", { name: "Open settings" });
    await user.click(trigger);
    const closeButton = screen.getByRole("button", { name: "Close settings" });
    expect(closeButton).toHaveFocus();

    await user.tab({ shift: true });
    expect(screen.getByRole("button", { name: "Cancel" })).toHaveFocus();
    await user.tab();
    expect(closeButton).toHaveFocus();

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Esc=\\e" } });
    const applyButton = screen.getByRole("button", { name: "Apply" });
    applyButton.focus();
    await user.tab();
    expect(closeButton).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Settings" })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("closes from the backdrop and restores trigger focus", async () => {
    const user = userEvent.setup();
    render(<SettingsHarness />);

    const trigger = screen.getByRole("button", { name: "Open settings" });
    await user.click(trigger);
    fireEvent.mouseDown(screen.getByRole("dialog", { name: "Settings" }).parentElement!);

    expect(screen.queryByRole("dialog", { name: "Settings" })).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });

  it("keeps incomplete shortcut input as a local draft without saving", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "Ctrl=" } });

    expect(textarea).toHaveValue("Ctrl=");
    expect(onPreferencesChange).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Apply" })).toBeDisabled();
  });

  it("applies a valid draft once without closing settings", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    const textarea = screen.getByRole("textbox");
    const draft = [
      "",
      "Equals=a=b",
      "EscUpper=\\u001B[5~",
      "EscLower=\\u001b[6~",
      "Backslash=\\\\",
      "Tab=\\t",
      "Return=\\r",
      "Line=\\n",
      "Literal=\\\\n",
      "",
    ].join("\n");
    fireEvent.change(textarea, { target: { value: draft } });

    expect(onPreferencesChange).not.toHaveBeenCalled();
    const applyButton = screen.getByRole("button", { name: "Apply" });
    expect(applyButton).toBeEnabled();
    await user.click(applyButton);

    expect(onPreferencesChange).toHaveBeenCalledTimes(1);
    expect(onPreferencesChange).toHaveBeenCalledWith({
      ...initialPreferences,
      mobileShortcuts: [
        { label: "Equals", data: "a=b" },
        { label: "EscUpper", data: "\x1b[5~" },
        { label: "EscLower", data: "\x1b[6~" },
        { label: "Backslash", data: "\\" },
        { label: "Tab", data: "\t" },
        { label: "Return", data: "\r" },
        { label: "Line", data: "\n" },
        { label: "Literal", data: "\\n" },
      ],
    });
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeInTheDocument();
    expect(applyButton).toBeDisabled();
  });

  it("preserves leading and trailing spaces in shortcut data", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    const draft = `Space=${" "}\nCommand=${"sudo "}\nIndent=${"  ls"}`;
    fireEvent.change(screen.getByRole("textbox"), { target: { value: draft } });
    await user.click(screen.getByRole("button", { name: "Apply" }));

    expect(onPreferencesChange).toHaveBeenCalledWith({
      ...initialPreferences,
      mobileShortcuts: [
        { label: "Space", data: " " },
        { label: "Command", data: "sudo " },
        { label: "Indent", data: "  ls" },
      ],
    });
    expect(screen.getByRole("textbox")).toHaveValue(draft);
  });

  it("applies an empty draft as an empty shortcut list", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    const initial: BrowserPreferences = {
      ...initialPreferences,
      mobileShortcuts: [{ label: "Esc", data: "\x1b" }],
    };
    render(<SettingsHarness initial={initial} onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "" } });
    await user.click(screen.getByRole("button", { name: "Apply" }));

    expect(onPreferencesChange).toHaveBeenCalledWith({ ...initial, mobileShortcuts: [] });
  });

  it("cancels a draft back to the current committed shortcuts and stays open", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    const initial: BrowserPreferences = {
      ...initialPreferences,
      mobileShortcuts: [{ label: "PgUp", data: "\x1b[5~" }],
    };
    render(<SettingsHarness initial={initial} onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Pending=abc" } });
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(screen.getByRole("textbox")).toHaveValue("PgUp=\\e[5~");
    expect(screen.getByRole("dialog", { name: "Settings" })).toBeInTheDocument();
    expect(onPreferencesChange).not.toHaveBeenCalled();
  });

  it("keeps the shortcut draft when another preference is saved immediately", async () => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Pending=abc" } });
    await user.click(screen.getByRole("radio", { name: "Dark" }));

    expect(screen.getByRole("textbox")).toHaveValue("Pending=abc");
    expect(onPreferencesChange).toHaveBeenCalledTimes(1);
    expect(onPreferencesChange).toHaveBeenCalledWith({ ...initialPreferences, theme: "dark" });
  });

  it.each(["close button", "backdrop", "Escape"])("discards a draft after closing with %s", async (closeMethod) => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Pending=abc" } });

    if (closeMethod === "close button") {
      await user.click(screen.getByRole("button", { name: "Close settings" }));
    } else if (closeMethod === "backdrop") {
      fireEvent.mouseDown(screen.getByRole("dialog", { name: "Settings" }).parentElement!);
    } else {
      await user.keyboard("{Escape}");
    }

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    expect(screen.getByRole("textbox")).toHaveValue("");
    expect(onPreferencesChange).not.toHaveBeenCalled();
  });

  it("reopens with the applied shortcuts", async () => {
    const user = userEvent.setup();
    render(<SettingsHarness />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Esc=\\u001b" } });
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await user.click(screen.getByRole("button", { name: "Close settings" }));
    await user.click(screen.getByRole("button", { name: "Open settings" }));

    expect(screen.getByRole("textbox")).toHaveValue("Esc=\\e");
  });

  it.each([
    ["missing separator", "Broken", "Line 1: use label=data."],
    ["empty label", "=value", "Line 1: label is required."],
    ["empty data", "Empty=", "Line 1: data is required."],
    ["long label", `${"L".repeat(13)}=x`, "Line 1: label must be 12 characters or fewer."],
    ["long decoded data", `Long=${"\\n".repeat(65)}`, "Line 1: decoded data must be 64 characters or fewer."],
    ["NUL data", "Nul=a\0b", "Line 1: data cannot contain NUL."],
    ["unknown escape", "Unknown=\\q", "Line 1: unsupported or incomplete escape sequence."],
    ["dangling escape", "Dangling=\\", "Line 1: unsupported or incomplete escape sequence."],
    ["too many shortcuts", Array.from({ length: 13 }, (_, index) => `K${index}=x`).join("\n"), "Line 13: no more than 12 shortcuts are allowed."],
  ])("shows an accessible inline error for %s", async (_caseName, draft, message) => {
    const user = userEvent.setup();
    const onPreferencesChange = vi.fn();
    render(<SettingsHarness onPreferencesChange={onPreferencesChange} />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: draft } });

    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent(message);
    expect(textarea).toHaveAttribute("aria-invalid", "true");
    expect(textarea).toHaveAttribute("aria-describedby", expect.stringContaining(alert.id));
    expect(screen.getByRole("button", { name: "Apply" })).toBeDisabled();
    expect(onPreferencesChange).not.toHaveBeenCalled();
  });

  it("reports the first invalid source line", async () => {
    const user = userEvent.setup();
    render(<SettingsHarness />);

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "\nGood=x\nBroken\nEmpty=" },
    });

    expect(screen.getByRole("alert")).toHaveTextContent("Line 3: use label=data.");
  });

  it("localizes shortcut actions and validation errors", async () => {
    const user = userEvent.setup();
    render(
      <I18nProvider locale="zh-CN">
        <SettingsHarness />
      </I18nProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Open settings" }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "错误" } });

    expect(screen.getByRole("button", { name: "取消" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "应用" })).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("第 1 行：请使用 标签=数据 格式。");
  });
});
