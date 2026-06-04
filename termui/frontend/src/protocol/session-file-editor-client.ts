import { ProtocolClientError } from "./direct-client";
import type {
  SessionFileReadResultPayload,
  SessionFileWrittenPayload,
  UUID,
} from "./types";
import { sessionDataFromBase64 } from "./wire";

export interface SessionFileEditorClient {
  close(): void;
  readSessionFile(
    sessionId: UUID,
    path: string,
    options?: { maxBytes?: number },
  ): Promise<SessionFileReadResultPayload>;
  writeSessionFile(
    sessionId: UUID,
    path: string,
    bytes: Uint8Array,
  ): Promise<SessionFileWrittenPayload>;
}

// 中文注释：浏览器 editor 只允许打开小文本文件。
// 这里把大小上限、base64 解码和二进制文件判定从页面组件里收口到 protocol helper。
export async function readEditableSessionFile(
  client: SessionFileEditorClient,
  sessionId: UUID,
  path: string,
  maxBytes: number,
): Promise<{ path: string; bytes: Uint8Array }> {
  const payload = await client.readSessionFile(sessionId, path, { maxBytes });
  if (payload.size_bytes > maxBytes) {
    throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
  }
  const bytes = sessionDataFromBase64(payload.data_base64);
  if (bytes.byteLength > maxBytes) {
    throw new ProtocolClientError("file_too_large", "file is too large to edit in browser");
  }
  if (bytes.includes(0)) {
    throw new ProtocolClientError("binary_file", "binary files cannot be edited in browser");
  }
  return { path: payload.path, bytes };
}
