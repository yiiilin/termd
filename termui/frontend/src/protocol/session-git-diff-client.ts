import type {
  SessionGitDiffResultPayload,
  UUID,
} from "./types";

// 中文注释：git diff viewer 当前只需要这一条只读能力外加 close；
// 先把 diff 面和 file/session 其它旁路能力彻底分开，避免再次把边界混回去。
export interface SessionGitDiffClient {
  close(): void;
  getSessionGitDiff(
    sessionId: UUID,
    worktreePath: string,
    filePath?: string | null,
    staged?: boolean,
  ): Promise<SessionGitDiffResultPayload>;
}
