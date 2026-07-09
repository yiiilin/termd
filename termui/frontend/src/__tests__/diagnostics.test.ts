import { afterEach, describe, expect, it, vi } from "vitest";
import { recordTermdDiagnostic, type TermdDiagnosticEvent } from "../diagnostics";

function testDiagnostics(): { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] } {
  return globalThis as { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] };
}

describe("诊断事件", () => {
  afterEach(() => {
    delete testDiagnostics().__TERMD_TRACE__;
    delete testDiagnostics().__TERMD_DIAG_EVENTS__;
    localStorage.clear();
    vi.restoreAllMocks();
  });

  it("记录终端输入诊断时丢弃 preview，只保留长度等元数据", () => {
    testDiagnostics().__TERMD_TRACE__ = true;
    testDiagnostics().__TERMD_DIAG_EVENTS__ = [];
    localStorage.setItem("termd.debug.trace.console", "1");
    const consoleDebug = vi.spyOn(console, "debug").mockImplementation(() => undefined);

    recordTermdDiagnostic("terminal_input", {
      chunkLength: 18,
      bufferedLength: 18,
      preview: "terminal-password",
    });

    const event = testDiagnostics().__TERMD_DIAG_EVENTS__?.at(-1);
    expect(event?.fields).toEqual({
      chunkLength: 18,
      bufferedLength: 18,
    });
    expect(JSON.stringify(event)).not.toContain("terminal-password");
    expect(JSON.stringify(consoleDebug.mock.calls)).not.toContain("terminal-password");
  });
});
