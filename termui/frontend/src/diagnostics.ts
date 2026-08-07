import { SENSITIVE_FIELD_KEY_PATTERN } from "./protocol/errors";

const TRACE_STORAGE_KEY = "termd.debug.trace";
const TRACE_CONSOLE_STORAGE_KEY = "termd.debug.trace.console";
const MAX_TRACE_EVENTS = 5000;
const DIAGNOSTIC_CONTEXT_ID = `page-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
/** 页面上下文起始的墙上时间（≈ timeOrigin），用于把 performance.now() 换算成日志时间戳。 */
const DIAGNOSTIC_CONTEXT_STARTED_AT = Date.now() - (typeof performance === "undefined" ? 0 : performance.now());

const UPLOAD_FLUSH_INTERVAL_MS = 2000;
const UPLOAD_FLUSH_MAX_EVENTS = 300;
const UPLOAD_QUEUE_MAX_EVENTS = 5000;

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

/** 上送分析日志的批量载荷，经 metadata WebSocket 以 `client.diagnostics` 消息发送。 */
export interface ClientDiagnosticsBatchPayload {
  context_id: string;
  context_started_at: number;
  events: TermdDiagnosticEvent[];
}

/** 具备上送能力的客户端；`sendClientDiagnostics` 返回是否真的发出了消息。 */
export interface ClientDiagnosticsSender {
  readonly isClosed: boolean;
  sendClientDiagnostics(payload: ClientDiagnosticsBatchPayload): boolean;
}

interface TermdDiagnosticGlobal {
  __TERMD_TRACE__?: boolean;
  __TERMD_DIAG_EVENTS__?: TermdDiagnosticEvent[];
}

let uploadEnabled = false;
let uploadTimer: ReturnType<typeof globalThis.setInterval> | undefined;
/** 已注册的上送 sender 栈（后注册者优先）；由各 client 在构造/关闭时维护。 */
const uploadSenders: ClientDiagnosticsSender[] = [];
let pendingUploadEvents: TermdDiagnosticEvent[] = [];

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

/** 设置「上送分析日志」开关（来自设置页）。开启后开始捕获与批量上送，关闭即停止并丢弃积压。 */
export function setDiagnosticUploadEnabled(enabled: boolean): void {
  if (uploadEnabled === enabled) {
    return;
  }
  uploadEnabled = enabled;
  if (enabled) {
    // 丢弃开启前积压的旧事件，避免把开启前的历史误报上去。
    pendingUploadEvents = [];
    uploadTimer = globalThis.setInterval(() => {
      flushDiagnosticUpload();
    }, UPLOAD_FLUSH_INTERVAL_MS);
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", handleUploadVisibilityChange);
      window.addEventListener("pagehide", handleUploadPageHide);
    }
  } else {
    if (uploadTimer !== undefined) {
      globalThis.clearInterval(uploadTimer);
      uploadTimer = undefined;
    }
    pendingUploadEvents = [];
    if (typeof document !== "undefined") {
      document.removeEventListener("visibilitychange", handleUploadVisibilityChange);
      window.removeEventListener("pagehide", handleUploadPageHide);
    }
  }
}

/**
 * 注册一个上送 sender（一般由 V070Client 在构造时调用）。
 * 返回注销函数；client 关闭时必须注销，避免向已关闭的 socket 反复发送。
 */
export function registerDiagnosticUploadSender(sender: ClientDiagnosticsSender): () => void {
  uploadSenders.push(sender);
  return () => {
    const index = uploadSenders.lastIndexOf(sender);
    if (index >= 0) {
      uploadSenders.splice(index, 1);
    }
  };
}

/** 立即 flush 待上送队列（定时器、队列满、页面隐藏时都会调用）。 */
export function flushDiagnosticUpload(): void {
  if (!uploadEnabled || pendingUploadEvents.length === 0) {
    return;
  }
  const events = pendingUploadEvents.splice(0, UPLOAD_FLUSH_MAX_EVENTS);
  const payload: ClientDiagnosticsBatchPayload = {
    context_id: DIAGNOSTIC_CONTEXT_ID,
    context_started_at: Math.round(DIAGNOSTIC_CONTEXT_STARTED_AT),
    events,
  };
  // 后注册的 sender 优先（通常是当前活跃 client）；只发给第一个真正发出的，
  // 避免同一批事件被多个并发 client 重复上送。
  for (let index = uploadSenders.length - 1; index >= 0; index -= 1) {
    const sender = uploadSenders[index];
    if (sender.isClosed) {
      continue;
    }
    if (sender.sendClientDiagnostics(payload)) {
      return;
    }
  }
  // 当前没有可用的 sender：把事件放回队首，等下次 flush 重试（队列有上限兜底）。
  pendingUploadEvents.unshift(...events);
  trimUploadQueue();
}

export function recordTermdDiagnostic(
  name: string,
  fields?: Record<string, unknown>,
  options: { stack?: boolean; console?: boolean } = {},
): void {
  const shouldTrace = traceEnabled();
  if (!shouldTrace && !options.console && !uploadEnabled) {
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
  if (uploadEnabled) {
    pendingUploadEvents.push(event);
    trimUploadQueue();
    if (pendingUploadEvents.length >= UPLOAD_FLUSH_MAX_EVENTS) {
      flushDiagnosticUpload();
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

function trimUploadQueue(): void {
  if (pendingUploadEvents.length > UPLOAD_QUEUE_MAX_EVENTS) {
    pendingUploadEvents.splice(0, pendingUploadEvents.length - UPLOAD_QUEUE_MAX_EVENTS);
  }
}

function handleUploadVisibilityChange(): void {
  if (document.visibilityState === "hidden") {
    flushDiagnosticUpload();
  }
}

function handleUploadPageHide(): void {
  flushDiagnosticUpload();
}

/** 递归过滤敏感键：命中 `token|secret|private|signature|ciphertext|authorization|bearer|preview` 的键整支丢弃。 */
function sanitizeDiagnosticFields(fields: Record<string, unknown>): Record<string, unknown> | undefined {
  const safeFields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (SENSITIVE_FIELD_KEY_PATTERN.test(key)) {
      continue;
    }
    const sanitized = sanitizeFieldValue(value);
    if (sanitized !== undefined) {
      safeFields[key] = sanitized;
    }
  }
  return Object.keys(safeFields).length ? safeFields : undefined;
}

function sanitizeFieldValue(value: unknown): unknown {
  if (Array.isArray(value)) {
    const items = value.map(sanitizeFieldValue).filter((item) => item !== undefined);
    return items.length ? items : undefined;
  }
  if (typeof value === "object" && value !== null) {
    const clean: Record<string, unknown> = {};
    for (const [key, nested] of Object.entries(value)) {
      if (SENSITIVE_FIELD_KEY_PATTERN.test(key)) {
        continue;
      }
      const sanitized = sanitizeFieldValue(nested);
      if (sanitized !== undefined) {
        clean[key] = sanitized;
      }
    }
    return Object.keys(clean).length ? clean : undefined;
  }
  return value;
}
