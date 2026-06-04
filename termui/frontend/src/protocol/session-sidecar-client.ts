import type {
  SessionFilesResultPayload,
  SessionGitResultPayload,
  UUID,
} from "./types";

// 中文注释：useSessionFileLoaders 当前只依赖文件树和 Git 读路径，
// 先把 hook 对全量 DirectClient 的类型耦合收窄到这两个只读能力面。
export interface SessionFileLoadersClient {
  listSessionFiles(sessionId: UUID, path?: string): Promise<SessionFilesResultPayload>;
  getSessionGit(sessionId: UUID): Promise<SessionGitResultPayload>;
}
