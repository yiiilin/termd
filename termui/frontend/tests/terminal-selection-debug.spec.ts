import { expect, test, type Page } from "@playwright/test";
import { MockDaemon } from "../src/test/mock-daemon";

async function activateButton(page: Page, name: string): Promise<void> {
  const button = page.getByRole("button", { name });
  await expect(button).toBeVisible();
  await expect(button).toBeEnabled();
  await button.focus();
  await expect(button).toBeFocused();
  await page.keyboard.press("Enter");
}

function pairingInviteCode(daemon: MockDaemon): string {
  const payload = JSON.stringify({
    type: "termd_pairing_qr",
    version: 1,
    token: "secret-token",
    server_id: daemon.serverId,
    daemon_public_key: daemon.daemonPublicKey,
    expires_at_ms: Date.now() + 60_000,
  });
  return `termd-pair:v1:${Buffer.from(payload, "utf8").toString("base64url")}`;
}

async function resetBrowserState(page: Page): Promise<void> {
  await page.addInitScript(() => {
    if (window.name === "__TERMD_TEST_STATE_RESET_DONE__") {
      return;
    }
    window.name = "__TERMD_TEST_STATE_RESET_DONE__";
    window.localStorage.clear();
    window.sessionStorage.clear();
    indexedDB.deleteDatabase("termd-termui-web");
  });
}

test("terminal lower-half drag selection returns the lower visible line", async ({ page }) => {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.addInitScript(() => {
    const scope = window as typeof window & {
      __termdClipboardWrites?: string[];
      __termdExecCopyCount?: number;
    };
    scope.__termdClipboardWrites = [];
    scope.__termdExecCopyCount = 0;
    const clipboard = navigator.clipboard;
    if (clipboard?.writeText) {
      const originalWriteText = clipboard.writeText.bind(clipboard);
      Object.defineProperty(clipboard, "writeText", {
        configurable: true,
        value: async (text: string) => {
          scope.__termdClipboardWrites?.push(text);
          return originalWriteText(text);
        },
      });
    }
    const originalExecCommand = document.execCommand.bind(document);
    document.execCommand = ((command: string) => {
      if (command === "copy") {
        scope.__termdExecCopyCount = (scope.__termdExecCopyCount ?? 0) + 1;
      }
      return originalExecCommand(command);
    }) as typeof document.execCommand;
  });

  const daemon = await MockDaemon.start({
    token: "secret-token",
    sessions: [
      {
        session_id: "00000000-0000-0000-0000-000000000531",
        state: "running",
        size: { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
      },
    ],
    attachOutput: Array.from({ length: 60 }, (_, index) => `line-${String(index + 1).padStart(3, "0")}\n`).join(""),
  });

  try {
    await resetBrowserState(page);
    await page.goto("/");
    await page.getByLabel("WS URL").fill(daemon.url);
    await page.getByLabel("Pairing token").fill(pairingInviteCode(daemon));
    await activateButton(page, "Pair");

    await expect.poll(async () => page.locator(".terminal-host").evaluate((host) => (host as HTMLElement).dataset.termdBuffer ?? "")).toContain("line-060");
    await page.locator(".terminal-pane").hover();

    const metrics = await page.locator(".terminal-host canvas").evaluate((canvas) => {
      const rect = (canvas as HTMLCanvasElement).getBoundingClientRect();
      const host = canvas.parentElement as HTMLElement;
      const bufferLineCount = (host.dataset.termdBuffer ?? "").split("\n").length;
      return {
        canvasLeft: rect.left,
        canvasTop: rect.top,
        canvasWidth: rect.width,
        canvasHeight: rect.height,
        hostScrollbackLength: Number.parseFloat(host.dataset.termdScrollbackLength ?? "0"),
        viewportRaw: Number.parseFloat(host.dataset.termdViewportYRaw ?? "0"),
        bufferLineCount,
        rows: Number.parseInt(host.dataset.termdRows ?? "0", 10),
        cols: Number.parseInt(host.dataset.termdCols ?? "0", 10),
      };
    });

    const visibleRows = Math.max(1, metrics.rows || Math.floor(metrics.bufferLineCount - metrics.hostScrollbackLength));
    const targetRow = Math.max(0, visibleRows - 5);
    const expectedAbsoluteLine = metrics.hostScrollbackLength - metrics.viewportRaw + targetRow + 1;
    const expectedToken = `line-${String(expectedAbsoluteLine).padStart(3, "0")}`;
    const rowHeight = metrics.canvasHeight / visibleRows;
    const cellWidth = metrics.canvasWidth / Math.max(1, metrics.cols || 80);
    // 中文注释：目标 token 从第 0 列开始；如果从画布中间拖拽，真实 xterm 会选中
    // 该行右侧空白并返回空字符串。这里仍在下半区测试，但横向命中实际文本列。
    const startX = metrics.canvasLeft + cellWidth * 0.2;
    const endX = metrics.canvasLeft + cellWidth * 8.2;
    const y = metrics.canvasTop + rowHeight * (targetRow + 0.52);
    const hitTargets = await page.evaluate(
      ({ startX, endX, y }) => {
        const describe = (x: number) => {
          const target = document.elementFromPoint(x, y);
          return target
            ? {
                tag: target.tagName,
                className: (target as HTMLElement).className,
                ariaLabel: (target as HTMLElement).getAttribute("aria-label"),
              }
            : null;
        };
        return { start: describe(startX), end: describe(endX) };
      },
      { startX, endX, y },
    );
    await page.mouse.move(startX, y);
    await page.mouse.down();
    await page.mouse.move(endX, y - rowHeight * 0.08);
    await page.mouse.up();

    await expect
      .poll(async () => page.evaluate(() => navigator.clipboard.readText()), { timeout: 2_000 })
      .not.toBe("");
    await page.screenshot({ path: "test-results/terminal-selection-debug.png", fullPage: true });
    const clipboardText = await page.evaluate(() => navigator.clipboard.readText());
    const clipboardDebug = await page.evaluate(() => {
      const scope = window as typeof window & {
        __termdClipboardWrites?: string[];
        __termdExecCopyCount?: number;
      };
      return {
        writes: scope.__termdClipboardWrites ?? [],
        execCopyCount: scope.__termdExecCopyCount ?? 0,
      };
    });
    const selectionDebug = await page.locator(".terminal-host").evaluate((host) => {
      const element = host as HTMLElement;
      return {
        hasSelection: element.dataset.termdHasSelection ?? "",
        selection: element.dataset.termdSelection ?? "",
        selectionPosition: element.dataset.termdSelectionPosition ?? "",
        selectionDragActive: element.dataset.termdSelectionDragActive ?? "",
        selectionDragDragging: element.dataset.termdSelectionDragDragging ?? "",
        selectionDragStart: element.dataset.termdSelectionDragStart ?? "",
        selectionDragLast: element.dataset.termdSelectionDragLast ?? "",
        selectionCopy: element.dataset.termdSelectionCopy ?? "",
      };
    });
    const postSelectionState = await page.locator(".terminal-host").evaluate((host) => {
      const element = host as HTMLElement;
      return {
        viewportRaw: Number.parseFloat(element.dataset.termdViewportYRaw ?? "0"),
        scrollbackLength: Number.parseFloat(element.dataset.termdScrollbackLength ?? "0"),
        selectionPosition: element.dataset.termdSelectionPosition ?? "",
      };
    });
    // 中文注释：expected 必须来自拖拽前肉眼可见的目标行，不能从 selectionPosition
    // 反推；否则 xterm 选错行时测试会跟着错误坐标移动。
    // 中文注释：xterm 内部 DOM 可能把命中点落在 canvas 本身，也可能落在覆盖其上的包装层。
    // 这里不把内部 tag 当成协议，只验证最终选中的文本确实来自目标可见行。
    expect(hitTargets.start).not.toBeNull();
    expect(hitTargets.end).not.toBeNull();
    expect(clipboardText || clipboardDebug.writes.at(-1) || "").toContain(expectedToken);
    expect(selectionDebug.selectionCopy).toContain(expectedToken);
    expect(selectionDebug.hasSelection).toBe("true");
    expect(postSelectionState.selectionPosition).not.toBe("");
  } finally {
    await daemon.stop();
  }
});
