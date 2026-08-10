//! 新版本探测：通过 GitHub Releases API 检查是否有比当前构建更新的版本。
//!
//! 探测结果缓存在 localStorage（1 小时）。**失败也写短期缓存**：未认证的
//! GitHub API 限流 60 次/小时/IP，多设备共享出口 IP 时若每刷新一页就重试，
//! 很容易触发 403，出现「刚才还有黄点、刷新后消失」的抖动。网络失败或限流
//! 时静默返回 undefined，不打扰用户。

const GITHUB_REPO = "yiiilin/termd";
const LATEST_RELEASE_API_URL = `https://api.github.com/repos/${GITHUB_REPO}/releases/latest`;
const CHECK_CACHE_KEY = "termd.update-check";
const CHECK_CACHE_TTL_MS = 60 * 60 * 1000;
const FETCH_TIMEOUT_MS = 8_000;

export interface VersionCheckResult {
  /** 最新 release 的版本号（不带 `v` 前缀），例如 `0.9.6`。 */
  latest: string;
  /** 最新 release 的 GitHub 页面地址。 */
  releaseUrl: string;
  checkedAtMs: number;
}

export function parseSemver(version: string): [number, number, number] | undefined {
  const match = /^v?(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/.exec(version.trim());
  if (!match) {
    return undefined;
  }
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

/** `latest` 是否严格大于 `current`（按 semver 数字段比较）。 */
export function isNewerVersion(latest: string, current: string): boolean {
  const latestParts = parseSemver(latest);
  const currentParts = parseSemver(current);
  if (!latestParts || !currentParts) {
    return false;
  }
  return (
    latestParts[0] > currentParts[0]
    || (latestParts[0] === currentParts[0] && latestParts[1] > currentParts[1])
    || (
      latestParts[0] === currentParts[0]
      && latestParts[1] === currentParts[1]
      && latestParts[2] > currentParts[2]
    )
  );
}

type CachedCheck =
  | { status: "valid"; result: VersionCheckResult }
  | { status: "cooldown" }
  | { status: "none" };

function readCachedCheck(): CachedCheck {
  if (typeof localStorage === "undefined") {
    return { status: "none" };
  }
  try {
    const raw = localStorage.getItem(CHECK_CACHE_KEY);
    if (!raw) {
      return { status: "none" };
    }
    const parsed = JSON.parse(raw) as {
      latest?: unknown;
      releaseUrl?: unknown;
      checkedAtMs?: unknown;
    };
    if (typeof parsed.checkedAtMs !== "number" || Date.now() - parsed.checkedAtMs >= CHECK_CACHE_TTL_MS) {
      return { status: "none" };
    }
    if (typeof parsed.latest !== "string" || parsed.latest.length === 0 || typeof parsed.releaseUrl !== "string") {
      // 上次检查失败留下的冷却缓存：TTL 内不再请求 GitHub API
      return { status: "cooldown" };
    }
    return {
      status: "valid",
      result: {
        latest: parsed.latest,
        releaseUrl: parsed.releaseUrl,
        checkedAtMs: parsed.checkedAtMs,
      },
    };
  } catch {
    return { status: "none" };
  }
}

function writeCachedCheck(result: VersionCheckResult | undefined): void {
  if (typeof localStorage === "undefined") {
    return;
  }
  try {
    const entry = result
      ? result
      : { latest: "", releaseUrl: "", checkedAtMs: Date.now() };
    localStorage.setItem(CHECK_CACHE_KEY, JSON.stringify(entry));
  } catch {
    // 存储失败（隐私模式等）不影响本次展示
  }
}

/**
 * 查询最新 release。优先返回未过期的本地缓存（`force` 时跳过缓存直接请求）；
 * 未过期的失败缓存（cooldown）直接返回 undefined 不再请求；网络/解析失败
 * 写冷却缓存并静默返回 undefined。
 */
export async function checkLatestRelease(options: { force?: boolean } = {}): Promise<VersionCheckResult | undefined> {
  const cached = readCachedCheck();
  if (cached.status === "valid" && !options.force) {
    return cached.result;
  }
  if (cached.status === "cooldown" && !options.force) {
    return undefined;
  }
  try {
    const response = await fetch(LATEST_RELEASE_API_URL, {
      headers: { accept: "application/vnd.github+json" },
      signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
    });
    if (!response.ok) {
      writeCachedCheck(undefined);
      return undefined;
    }
    const body = (await response.json()) as { tag_name?: unknown; html_url?: unknown };
    if (typeof body.tag_name !== "string" || typeof body.html_url !== "string") {
      writeCachedCheck(undefined);
      return undefined;
    }
    const latest = body.tag_name.trim().replace(/^v/, "");
    if (!parseSemver(latest)) {
      writeCachedCheck(undefined);
      return undefined;
    }
    const result: VersionCheckResult = {
      latest,
      releaseUrl: body.html_url,
      checkedAtMs: Date.now(),
    };
    writeCachedCheck(result);
    return result;
  } catch {
    writeCachedCheck(undefined);
    return undefined;
  }
}
