import type {
  SessionFileDeletedPayload,
  SessionGitActionKind,
  SessionGitActionResultPayload,
  UUID,
} from "./types";

// 中文注释：短 RPC mutation 只需要最小的 git/file 修改面；
// 后续 upload/download 继续单独拆，不要把长传输耦合进来。
export interface SessionMutationClient {
  close(): void;
  applySessionGitAction(
    sessionId: UUID,
    worktreePath: string,
    filePath: string,
    action: SessionGitActionKind,
  ): Promise<SessionGitActionResultPayload>;
  deleteSessionFile(sessionId: UUID, path: string): Promise<SessionFileDeletedPayload>;
}
