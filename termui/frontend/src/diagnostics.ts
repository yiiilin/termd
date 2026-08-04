const TRACE_STORAGE_KEY = "termd.debug.trace";
const TRACE_CONSOLE_STORAGE_KEY = "termd.debug.trace.console";
const MAX_TRACE_EVENTS = 5000;
const DIAGNOSTIC_CONTEXT_ID = `page-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;

export interface TermdDiagnosticEvent {
  t: number;
  name: string;
  fields?: Record<string, unknown>;
  stack?: string;
}

export interface ProtocolTimeoutDiagnosticFields {
  layer: "client" | "relay" | "termd" | "supervisor";
  phase: string;
  timeout_code: string;
  timeout_ms: number;
  elapsed_ms?: number;
  transport?: string;
  method?: string;
  request_id?: string;
  stream_id?: string;
  session_id?: string;
  server_id?: string;
  device_id?: string;
  path?: string;
  role?: string;
  [key: string]: unknown;
}

interface TermdDiagnosticGlobal {
  __TERMD_TRACE__?: boolean;
  __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[];
}

export function termdDiagnosticContextId(): string {
  return DIAGNOSTIC_CONTEXT_ID;
}

function traceGlobal(): TermdDiagnosticGlobal {
  return globalThis as TermdDiagnosticGlobal;
}

function traceEnabled(): boolean {
  if (traceGlobal().__TERMD_TRACE__ === true) {
    return true;
  }
  if (typeof localStorage === "undefined") {
    return false;
  }
  return localStorage.getItem(TRACE_STORAGE_KEY) === "1";
}

function traceConsoleEnabled(): boolean {
  if (typeof localStorage === "undefined") {
    return false;
  }
  return localStorage.getItem(TRACE_CONSOLE_STORAGE_KEY) === "1";
}

export function recordTermdDiagnostic(
  name: string,
  fields?: Record<string, unknown>,
  options: { stack?: boolean; console?: boolean } = {},
): void {
  const shouldTrace = traceEnabled();
  if (!shouldTrace && !options.console) {
    return;
  }
  const safeFields = fields ? sanitizeDiagnosticFields(fields) : undefined;
  const event: TermdDiagnosticEvent = {
    t: typeof performance === "undefined" ? Date.now() : performance.now(),
    name,
    ...(safeFields ? { fields: safeFields } : {}),
    ...(options.stack ? { stack: new Error(name).stack } : {}),
  };
  if (shouldTrace) {
    const target = traceGlobal();
    const events = target.__TERMD_DIAG_EVENTS__ ?? [];
    target.__TERMD_DIAG_EVENTS__ = events;
    events.push(event);
    if (events.length > MAX_TRACE_EVENTS) {
      events.splice(0, events.length - MAX_TRACE_EVENTS);
    }
  }
  if (options.console || traceConsoleEnabled()) {
    // 中文注释：诊断日志默认只保存在内存数组里；显式开启 console 开关时才打印。
    // terminal 连接边界会强制输出，便于现场区分主动替换、服务端 close 和网络断开。
    if (options.console) {
      // eslint-disable-next-line no-console
      console.info("[termd-terminal]", name, safeFields ?? {}, ...(event.stack ? [event.stack] : []));
    } else {
      // eslint-disable-next-line no-console
      console.debug("[termd-trace]", name, safeFields ?? {}, ...(event.stack ? [event.stack] : []));
    }
  }
}

export function recordProtocolTimeout(fields: ProtocolTimeoutDiagnosticFields): void {
  recordTermdDiagnostic("protocol_timeout", fields);
}

function sanitizeDiagnosticFields(fields: Record<string, unknown>): Record<string, unknown> | undefined {
  const safeFields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (key.toLowerCase().includes("preview")) {
      continue;
    }
    safeFields[key] = value;
  }
  return Object.keys(safeFields).length ? safeFields : undefined;
}
