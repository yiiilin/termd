import type { SessionSummaryPayload, UUID } from "./protocol/types";

const FALLBACK_SESSION_NAMES = [
  "Ada",
  "Aristotle",
  "Babbage",
  "Bohr",
  "Byron",
  "Cantor",
  "Curie",
  "Dijkstra",
  "Dirac",
  "Euclid",
  "Euler",
  "Faraday",
  "Fermi",
  "Fourier",
  "Franklin",
  "Galileo",
  "Gauss",
  "Gibbs",
  "Hamilton",
  "Hamming",
  "Hilbert",
  "Hopper",
  "Kant",
  "Kepler",
  "Knuth",
  "Lagrange",
  "Laplace",
  "Lovelace",
  "Maxwell",
  "Newton",
  "Noether",
  "Pascal",
  "Planck",
  "Plato",
  "Ramanujan",
  "Rawls",
  "Riemann",
  "Russell",
  "Shannon",
  "Socrates",
  "Tesla",
  "Turing",
  "Volta",
  "Weyl",
];

export function sessionDisplayName(session: Pick<SessionSummaryPayload, "session_id" | "name">): string {
  const name = session.name?.trim();
  if (name) {
    return name;
  }
  return fallbackSessionDisplayName(session.session_id);
}

export function fallbackSessionDisplayName(sessionId: UUID): string {
  return FALLBACK_SESSION_NAMES[stableNameIndex(sessionId) % FALLBACK_SESSION_NAMES.length];
}

function stableNameIndex(sessionId: UUID): number {
  // 旧 daemon 或迁移数据可能没有名称；这里按 session_id 做稳定兜底，避免列表顺序改变时改名。
  let hash = 2_166_136_261;
  for (let index = 0; index < sessionId.length; index += 1) {
    hash = Math.imul(hash ^ sessionId.charCodeAt(index) ^ Math.imul(index, 31), 16_777_619) >>> 0;
  }
  return hash;
}
