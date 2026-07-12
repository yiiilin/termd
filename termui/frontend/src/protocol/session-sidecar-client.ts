import type {
  SessionFilesResultPayload,
  SessionGitResultPayload,
  UUID,
} from "./types";

// 中文注释：useSessionFileLoaders 当前只依赖文件树和 Git 读路径，
// Hooks depend only on the read capabilities they actually use.
export interface SessionFileLoadersClient {
  listSessionFiles(sessionId: UUID, path?: string): Promise<SessionFilesResultPayload>;
  getSessionGit(sessionId: UUID): Promise<SessionGitResultPayload>;
}
