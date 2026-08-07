import { afterEach, describe, expect, it, vi } from "vitest";
import { checkLatestRelease, isNewerVersion, parseSemver } from "../version-check";

afterEach(() => {
  localStorage.clear();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("parseSemver", () => {
  it("解析标准版本与 v 前缀", () => {
    expect(parseSemver("0.9.6")).toEqual([0, 9, 6]);
    expect(parseSemver("v0.9.6")).toEqual([0, 9, 6]);
    expect(parseSemver("0.10.0")).toEqual([0, 10, 0]);
  });

  it("拒绝非法版本", () => {
    expect(parseSemver("")).toBeUndefined();
    expect(parseSemver("latest")).toBeUndefined();
    expect(parseSemver("0.9")).toBeUndefined();
    expect(parseSemver("v1.2")).toBeUndefined();
  });
});

describe("isNewerVersion", () => {
  it("按 semver 数字段比较", () => {
    expect(isNewerVersion("0.9.7", "0.9.6")).toBe(true);
    expect(isNewerVersion("0.10.0", "0.9.6")).toBe(true);
    expect(isNewerVersion("1.0.0", "0.9.6")).toBe(true);
    expect(isNewerVersion("0.9.6", "0.9.6")).toBe(false);
    expect(isNewerVersion("0.9.5", "0.9.6")).toBe(false);
    expect(isNewerVersion("0.10.0", "0.10.0")).toBe(false);
  });

  it("非法版本返回 false", () => {
    expect(isNewerVersion("garbage", "0.9.6")).toBe(false);
    expect(isNewerVersion("0.9.7", "garbage")).toBe(false);
  });
});

describe("checkLatestRelease", () => {
  it("获取最新 release 并写入缓存", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      tag_name: "v0.10.0",
      html_url: "https://github.com/yiiilin/termd/releases/tag/0.10.0",
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const result = await checkLatestRelease();
    expect(result).toEqual({
      latest: "0.10.0",
      releaseUrl: "https://github.com/yiiilin/termd/releases/tag/0.10.0",
      checkedAtMs: expect.any(Number),
    });
    expect(localStorage.getItem("termd.update-check")).toContain("0.10.0");
  });

  it("缓存命中时不再请求 GitHub API", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      tag_name: "v0.10.0",
      html_url: "https://github.com/yiiilin/termd/releases/tag/0.10.0",
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    await checkLatestRelease();
    await checkLatestRelease();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("失败写入冷却缓存，TTL 内不再重试（避免限流抖动）", async () => {
    const fetchMock = vi.fn(async () => new Response("rate limited", { status: 403 }));
    vi.stubGlobal("fetch", fetchMock);

    expect(await checkLatestRelease()).toBeUndefined();
    expect(await checkLatestRelease()).toBeUndefined();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("网络异常静默返回 undefined 并进入冷却", async () => {
    const fetchMock = vi.fn(async () => { throw new Error("network down"); });
    vi.stubGlobal("fetch", fetchMock);

    expect(await checkLatestRelease()).toBeUndefined();
    expect(await checkLatestRelease()).toBeUndefined();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("响应缺少 tag_name 时不展示结果并进入冷却", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({}), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    expect(await checkLatestRelease()).toBeUndefined();
    expect(await checkLatestRelease()).toBeUndefined();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});
