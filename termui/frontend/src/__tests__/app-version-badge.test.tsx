import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AppVersionBadge } from "../components/AppVersionBadge";
import { I18nProvider } from "../i18n";
import packageMetadata from "../../package.json";

function installFetchMock(routes: Array<{ match: RegExp; response: Response }>) {
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    const route = routes.find((candidate) => candidate.match.test(url));
    if (!route) {
      throw new Error(`unexpected fetch: ${url}`);
    }
    return route.response;
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

function releaseResponse(tag: string, arch = "termd") {
  return new Response(JSON.stringify({
    tag_name: tag,
    html_url: `https://github.com/yiiilin/termd/releases/tag/${tag.replace(/^v/, "")}`,
    assets: [{ name: `${arch}-linux-amd64`, browser_download_url: `https://github.com/yiiilin/termd/releases/download/${tag}/${arch}-linux-amd64` }],
  }), { status: 200 });
}

function renderBadge(props: {
  termdVersion?: string;
  onUpdateTermd?: () => Promise<boolean>;
  onUpdateRelay?: () => Promise<boolean>;
} = {}) {
  return render(
    <I18nProvider locale="en-US">
      <AppVersionBadge {...props} />
    </I18nProvider>,
  );
}

async function settleCheck() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(3100);
  });
}

async function openDialog(badge: HTMLElement) {
  fireEvent.click(badge);
  await act(async () => {
    await Promise.resolve();
  });
}

afterEach(() => {
  localStorage.clear();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("AppVersionBadge", () => {
  it("直连场景：显示 termd 版本，无更新时无黄点，弹窗只有 Termd 行", async () => {
    vi.useFakeTimers();
    installFetchMock([
      { match: /api\.github\.com/, response: releaseResponse(`v${packageMetadata.version}`) },
      // 直连 daemon：/version 返回 termd 组件，不显示 Relay 行
      { match: /\/version$/, response: new Response(JSON.stringify({ component: "termd", version: packageMetadata.version }), { status: 200 }) },
    ]);
    renderBadge({ termdVersion: packageMetadata.version });

    const badge = screen.getByRole("button", { name: "Version update" });
    expect(within(badge).getByText(`v${packageMetadata.version}`)).toBeInTheDocument();
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Termd")).toBeInTheDocument();
    expect(within(dialog).queryByText("Relay")).toBeNull();
    expect(within(dialog).getByText("You are on the latest version.")).toBeInTheDocument();
  });

  it("termd 有新版本时显示黄点，点击更新调用已认证回调", async () => {
    vi.useFakeTimers();
    installFetchMock([
      { match: /api\.github\.com/, response: releaseResponse("v99.0.0") },
      { match: /\/version$/, response: new Response(JSON.stringify({ component: "termd", version: packageMetadata.version }), { status: 200 }) },
    ]);
    const onUpdateTermd = vi.fn(async () => true);
    renderBadge({ termdVersion: packageMetadata.version, onUpdateTermd });

    const badge = screen.getByRole("button", { name: "Version update" });
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).not.toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText(/v99\.0\.0/)).toBeInTheDocument();
    const updateButton = within(dialog).getByRole("button", { name: "Update now" });
    fireEvent.click(updateButton);
    await act(async () => {
      await Promise.resolve();
    });
    expect(onUpdateTermd).toHaveBeenCalledTimes(1);
    expect(within(dialog).getByText(/restarting/i)).toBeInTheDocument();
  });

  it("经 relay 场景：/version 返回 termrelay 时显示 Relay 行，两个组件各自可更新", async () => {
    vi.useFakeTimers();
    installFetchMock([
      { match: /api\.github\.com/, response: releaseResponse("v0.9.7") },
      { match: /\/version$/, response: new Response(JSON.stringify({ component: "termrelay", version: "0.9.5" }), { status: 200 }) },
    ]);
    const onUpdateTermd = vi.fn(async () => true);
    const onUpdateRelay = vi.fn(async () => true);
    renderBadge({ termdVersion: "0.9.6", onUpdateTermd, onUpdateRelay });

    const badge = screen.getByRole("button", { name: "Version update" });
    await settleCheck();
    // relay 0.9.5 < latest 0.9.7：黄点出现
    expect(badge.querySelector(".app-version-dot")).not.toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Termd")).toBeInTheDocument();
    expect(within(dialog).getByText("Relay")).toBeInTheDocument();
    expect(within(dialog).getByText(/v0\.9\.5 → v0\.9\.7/)).toBeInTheDocument();
    const buttons = within(dialog).getAllByRole("button", { name: "Update now" });
    expect(buttons).toHaveLength(2);

    fireEvent.click(buttons[1]);
    await act(async () => {
      await Promise.resolve();
    });
    expect(onUpdateRelay).toHaveBeenCalledTimes(1);
    expect(onUpdateTermd).not.toHaveBeenCalled();
  });

  it("探测失败不显示黄点，弹窗提示无法获取最新版本", async () => {
    vi.useFakeTimers();
    installFetchMock([
      { match: /api\.github\.com/, response: new Response("rate limited", { status: 403 }) },
      { match: /\/version$/, response: new Response(JSON.stringify({ component: "termd", version: packageMetadata.version }), { status: 200 }) },
    ]);
    renderBadge();

    const badge = screen.getByRole("button", { name: "Version update" });
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Could not fetch the latest version.")).toBeInTheDocument();
  });

  it("探测完成前点击弹窗显示检查中状态", async () => {
    vi.useFakeTimers();
    renderBadge();

    await openDialog(screen.getByRole("button", { name: "Version update" }));
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Checking for updates…")).toBeInTheDocument();
  });

  it("页面打开期间每小时重新探测一次（缓存过期后）", async () => {
    vi.useFakeTimers();
    const fetchMock = installFetchMock([
      { match: /api\.github\.com/, response: releaseResponse("v0.9.6") },
      { match: /\/version$/, response: new Response(JSON.stringify({ component: "termd", version: "0.9.6" }), { status: 200 }) },
    ]);
    renderBadge({ termdVersion: "0.9.6" });

    await settleCheck();
    const githubCalls = () => fetchMock.mock.calls.filter(([input]) => String(input).includes("api.github.com")).length;
    expect(githubCalls()).toBe(1);

    // 一小时内：缓存仍有效，定时器触发也只复用缓存
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60 * 1000);
    });
    expect(githubCalls()).toBe(1);

    // 缓存过期后（TTL 1h 边界），定时器触发真实请求
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60 * 1000);
    });
    expect(githubCalls()).toBe(2);
  });
});
