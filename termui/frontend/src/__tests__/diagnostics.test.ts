import { afterEach, describe, expect, it, vi } from "vitest";
import {
  flushDiagnosticUpload,
  recordTermdDiagnostic,
  registerDiagnosticUploadSender,
  setDiagnosticUploadEnabled,
  type ClientDiagnosticsBatchPayload,
  type ClientDiagnosticsSender,
  type TermdDiagnosticEvent,
} from "../diagnostics";

function testDiagnostics(): { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] } {
  return globalThis as { __TERMD_TRACE__?: boolean; __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[] };
}

function fakeSender(options: { open?: boolean; sendResult?: boolean } = {}): ClientDiagnosticsSender & {
  batches: ClientDiagnosticsBatchPayload[];
} {
  const { open = true, sendResult = true } = options;
  const sender: ClientDiagnosticsSender & { batches: ClientDiagnosticsBatchPayload[] } = {
    isClosed: !open,
    batches: [],
    sendClientDiagnostics(payload: ClientDiagnosticsBatchPayload) {
      if (!open || !sendResult) return false;
      sender.batches.push(payload);
      return true;
    },
  };
  return sender;
}

const senderCleanups: Array<() => void> = [];

function registerFake(sender: ClientDiagnosticsSender): void {
  senderCleanups.push(registerDiagnosticUploadSender(sender));
}

describe("诊断事件", () => {
  afterEach(() => {
    delete testDiagnostics().__TERMD_TRACE__;
    delete testDiagnostics().__TERMD_DIAG_EVENTS__;
    localStorage.clear();
    setDiagnosticUploadEnabled(false);
    for (const cleanup of senderCleanups.splice(0)) cleanup();
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

  it("关键连接事件可在未开启完整 trace 时直接输出到 console", () => {
    const consoleInfo = vi.spyOn(console, "info").mockImplementation(() => undefined);

    recordTermdDiagnostic("terminal_socket_closed", {
      connectionId: "terminal-1",
      preview: "must-not-leak",
      code: 1006,
    }, { console: true });

    expect(testDiagnostics().__TERMD_DIAG_EVENTS__).toBeUndefined();
    expect(consoleInfo).toHaveBeenCalledWith(
      "[termd-terminal]",
      "terminal_socket_closed",
      { connectionId: "terminal-1", code: 1006 },
    );
  });

  it("开启上送后把事件按批量交给活跃 sender", () => {
    setDiagnosticUploadEnabled(true);
    const sender = fakeSender();
    registerFake(sender);

    recordTermdDiagnostic("terminal_pane_output_reset", { outputResetVersion: 2 });
    recordTermdDiagnostic("terminal_writer_sequence_gap", { sequenceCursor: 3, expected: 4 });
    flushDiagnosticUpload();

    expect(sender.batches).toHaveLength(1);
    const batch = sender.batches[0];
    expect(batch.events.map((event) => event.name)).toEqual([
      "terminal_pane_output_reset",
      "terminal_writer_sequence_gap",
    ]);
    expect(batch.context_id).toMatch(/^page-/);
    expect(batch.context_started_at).toBeGreaterThan(1_700_000_000_000);
  });

  it("开启前的事件不会上送，关闭后停止捕获并丢弃积压", () => {
    recordTermdDiagnostic("before_enable", {});
    setDiagnosticUploadEnabled(true);
    const sender = fakeSender();
    registerFake(sender);
    flushDiagnosticUpload();
    expect(sender.batches).toHaveLength(0);

    recordTermdDiagnostic("while_enabled", {});
    setDiagnosticUploadEnabled(false);
    recordTermdDiagnostic("after_disable", {});
    flushDiagnosticUpload();
    expect(sender.batches).toHaveLength(0);
  });

  it("没有可用 sender 时事件留在队列，等 sender 出现后重试", () => {
    setDiagnosticUploadEnabled(true);
    const closedSender = fakeSender({ open: false });
    registerFake(closedSender);

    recordTermdDiagnostic("pending_event", {});
    flushDiagnosticUpload();
    expect(closedSender.batches).toHaveLength(0);

    const openSender = fakeSender();
    registerFake(openSender);
    flushDiagnosticUpload();
    expect(openSender.batches).toHaveLength(1);
    expect(openSender.batches[0].events.map((event) => event.name)).toEqual(["pending_event"]);
  });

  it("优先使用最近注册的 sender，发送失败时回退到更早的 sender", () => {
    setDiagnosticUploadEnabled(true);
    const first = fakeSender();
    const failing = fakeSender({ sendResult: false });
    registerFake(first);
    registerFake(failing);

    recordTermdDiagnostic("routed_event", {});
    flushDiagnosticUpload();

    expect(first.batches).toHaveLength(1);
    expect(failing.batches).toHaveLength(0);
  });

  it("上送前递归剥离敏感键（含嵌套对象与数组）", () => {
    setDiagnosticUploadEnabled(true);
    const sender = fakeSender();
    registerFake(sender);

    recordTermdDiagnostic("with_secrets", {
      ok: 1,
      preview: "must-not-leak",
      access_token: "must-not-leak",
      nested: { inner_token: "must-not-leak", keep: "value" },
      list: [{ signature: "must-not-leak", ok: 2 }, 3, { bearer: "must-not-leak" }],
    });
    flushDiagnosticUpload();

    const payload = JSON.stringify(sender.batches[0]);
    expect(payload).not.toContain("must-not-leak");
    expect(sender.batches[0].events[0].fields).toEqual({
      ok: 1,
      nested: { keep: "value" },
      list: [{ ok: 2 }, 3],
    });
  });
});
