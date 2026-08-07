import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AppVersionBadge } from "../components/AppVersionBadge";
import { I18nProvider } from "../i18n";
import packageMetadata from "../../package.json";

function renderBadge() {
  return render(
    <I18nProvider locale="en-US">
      <AppVersionBadge />
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
  it("显示当前版本；探测无更新时不显示黄点，弹窗展示当前版本与 GitHub 链接", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({
      tag_name: packageMetadata.version,
      html_url: `https://github.com/yiiilin/termd/releases/tag/${packageMetadata.version}`,
    }), { status: 200 })));
    renderBadge();

    const badge = screen.getByRole("button", { name: "Version update" });
    expect(within(badge).getByText(`v${packageMetadata.version}`)).toBeInTheDocument();
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Current version")).toBeInTheDocument();
    expect(within(dialog).getByText(`v${packageMetadata.version}`)).toBeInTheDocument();
    expect(within(dialog).getByText("You are on the latest version.")).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: "View GitHub release" }))
      .toHaveAttribute("href", `https://github.com/yiiilin/termd/releases/tag/${packageMetadata.version}`);
  });

  it("发现新版本时显示黄色小点，弹窗展示当前与最新版本", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({
      tag_name: "v99.0.0",
      html_url: "https://github.com/yiiilin/termd/releases/tag/99.0.0",
    }), { status: 200 })));
    renderBadge();

    const badge = screen.getByRole("button", { name: "Version update" });
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).not.toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Latest version")).toBeInTheDocument();
    expect(within(dialog).getByText("v99.0.0")).toBeInTheDocument();
    expect(within(dialog).getByText("A new version is available.")).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: "View GitHub release" }))
      .toHaveAttribute("href", "https://github.com/yiiilin/termd/releases/tag/99.0.0");
  });

  it("探测失败不显示黄点，弹窗提示无法获取最新版本", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("fetch", vi.fn(async () => { throw new Error("offline"); }));
    renderBadge();

    const badge = screen.getByRole("button", { name: "Version update" });
    await settleCheck();
    expect(badge.querySelector(".app-version-dot")).toBeNull();

    await openDialog(badge);
    const dialog = screen.getByRole("dialog", { name: "Version update" });
    expect(within(dialog).getByText("Could not fetch the latest version.")).toBeInTheDocument();
    // 失败时 GitHub 链接仍可用（跳转 releases 列表页）
    expect(within(dialog).getByRole("link", { name: "View GitHub release" }))
      .toHaveAttribute("href", "https://github.com/yiiilin/termd/releases");
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
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({
      tag_name: "v0.9.6",
      html_url: "https://github.com/yiiilin/termd/releases/tag/0.9.6",
    }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    renderBadge();

    await settleCheck();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // 一小时内：缓存仍有效，定时器触发也只复用缓存，不重复请求
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60 * 1000);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // 再过一个小时：缓存过期（TTL 1h），定时器触发真实请求
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60 * 1000);
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);

    // 新版本发布后，黄点在一个小时窗口内出现
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({
      tag_name: "v99.0.0",
      html_url: "https://github.com/yiiilin/termd/releases/tag/99.0.0",
    }), { status: 200 }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60 * 60 * 1000);
    });
    const badge = screen.getByRole("button", { name: "Version update" });
    expect(badge.querySelector(".app-version-dot")).not.toBeNull();
  });
});
