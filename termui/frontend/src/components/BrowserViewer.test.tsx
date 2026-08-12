import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { BrowserState, FileOfferPayload } from "../protocol/types";
import BrowserViewer, {
  browserViewerKeyToKeysym,
  forwardBrowserViewerComposition,
  forwardBrowserViewerKey,
} from "./BrowserViewer";

const viewerMocks = vi.hoisted(() => ({
  instances: [] as Array<EventTarget & {
    url: string;
    options: { wsProtocols?: string[] };
    disconnect: ReturnType<typeof vi.fn>;
  }>,
  accessToken: vi.fn<() => Promise<string>>(),
  close: vi.fn(),
  dispose: vi.fn(),
  metadataClose: vi.fn(),
  prepareFileOfferDownload: vi.fn(),
  fileOfferDownloadUrl: vi.fn(),
  subscribeMetadata: vi.fn(),
  fileOfferListener: undefined as ((offer: FileOfferPayload) => void) | undefined,
  loadState: vi.fn<() => Promise<BrowserState>>(),
}));

vi.mock("@novnc/novnc", () => ({
  default: class MockRFB extends EventTarget {
    background = "";
    clipViewport = false;
    compressionLevel = 0;
    dragViewport = false;
    focusOnClick = false;
    qualityLevel = 0;
    resizeSession = false;
    scaleViewport = false;
    viewOnly = false;
    disconnect = vi.fn();
    sendCtrlAltDel = vi.fn();
    sendKey = vi.fn();

    constructor(_target: HTMLElement, readonly url: string, readonly options: { wsProtocols?: string[] }) {
      super();
      viewerMocks.instances.push(this);
    }
  },
}));

vi.mock("../protocol/browser-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../protocol/browser-client")>();
  return {
    ...actual,
    BrowserWorkspaceClient: class {
      accessToken = viewerMocks.accessToken;
      close = viewerMocks.close;
      dispose = viewerMocks.dispose;
    },
  };
});

vi.mock("../state/browser-state", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../state/browser-state")>();
  return { ...actual, loadBrowserState: viewerMocks.loadState };
});

vi.mock("../protocol/v070-client", () => ({
  V070Client: class {
    close = viewerMocks.metadataClose;
    prepareFileOfferDownload = viewerMocks.prepareFileOfferDownload;
    fileOfferDownloadUrl = viewerMocks.fileOfferDownloadUrl;
    subscribeMetadata = viewerMocks.subscribeMetadata;

    watchFileOffers(listener: NonNullable<typeof viewerMocks.fileOfferListener>) {
      viewerMocks.fileOfferListener = listener;
      return () => {
        if (viewerMocks.fileOfferListener === listener) viewerMocks.fileOfferListener = undefined;
      };
    }
  },
}));

const browserId = "11111111-2222-4333-8444-555555555555";
const serverId = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

describe("BrowserViewer", () => {
  beforeEach(() => {
    viewerMocks.instances.length = 0;
    viewerMocks.accessToken.mockResolvedValue("header.payload.signature");
    viewerMocks.close.mockResolvedValue(undefined);
    viewerMocks.fileOfferListener = undefined;
    viewerMocks.metadataClose.mockReset();
    viewerMocks.prepareFileOfferDownload.mockReset();
    viewerMocks.fileOfferDownloadUrl.mockReset();
    viewerMocks.subscribeMetadata.mockReset().mockResolvedValue(undefined);
    viewerMocks.loadState.mockResolvedValue({
      device: {
        device_id: "99999999-8888-4777-8666-555555555555",
        device_public_key: "device-public",
        device_signing_key_secret: "device-secret",
      },
      pairedServers: [{
        server_id: serverId,
        daemon_public_key: "daemon-public",
        device_certificate: "certificate",
        url: "wss://relay.example/ws",
        paired_at_ms: 1,
      }],
    });
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: vi.fn(() => ({
        matches: true,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      })),
    });
  });

  it("connects noVNC with the RFB protocol and a fresh access token", async () => {
    const { unmount } = render(<BrowserViewer browserId={browserId} serverId={serverId} />);

    await waitFor(() => expect(viewerMocks.instances).toHaveLength(1));
    const rfb = viewerMocks.instances[0];
    expect(rfb.url).toBe(`wss://relay.example/ws/browser/${browserId}`);
    expect(rfb.options.wsProtocols).toEqual(["termd.rfb.v1", "header.payload.signature"]);

    act(() => rfb.dispatchEvent(new Event("connect")));
    expect(screen.getByRole("status")).toHaveTextContent("Connected");

    unmount();
    expect(rfb.disconnect).toHaveBeenCalledOnce();
    expect(viewerMocks.dispose).toHaveBeenCalledOnce();
  });

  it("shows completed browser downloads through the existing File Offer prompt", async () => {
    viewerMocks.prepareFileOfferDownload.mockResolvedValue({
      download_id: "77777777-6666-4555-8444-333333333333",
      download_url: "/api/files/offer-downloads/77777777-6666-4555-8444-333333333333",
      name: "report.zip",
      size_bytes: 13,
      expires_at_ms: Date.now() + 60_000,
    });
    viewerMocks.fileOfferDownloadUrl.mockReturnValue(
      "https://relay.example/api/files/offer-downloads/77777777-6666-4555-8444-333333333333",
    );
    const nativeClick = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
    const { unmount } = render(<BrowserViewer browserId={browserId} serverId={serverId} />);
    await waitFor(() => expect(viewerMocks.fileOfferListener).toBeTypeOf("function"));

    act(() => viewerMocks.fileOfferListener?.({
      offer_id: "12345678-1234-4234-8234-123456789abc",
      name: "report.zip",
      path: "/var/tmp/termd-browser-downloads/session/report.zip",
      size_bytes: 13,
      created_at_ms: Date.now(),
      expires_at_ms: Date.now() + 86_400_000,
    }));

    const download = await screen.findByRole("button", { name: /download report\.zip/i });
    fireEvent.click(download);
    await waitFor(() => expect(viewerMocks.prepareFileOfferDownload).toHaveBeenCalledWith(
      "12345678-1234-4234-8234-123456789abc",
    ));
    expect(nativeClick).toHaveBeenCalledOnce();

    unmount();
    nativeClick.mockRestore();
  });

  it("retries the File Offer metadata connection after its first attempt fails", async () => {
    let rejectFirstAttempt: ((reason?: unknown) => void) | undefined;
    viewerMocks.subscribeMetadata
      .mockImplementationOnce(() => new Promise<void>((_resolve, reject) => {
        rejectFirstAttempt = reject;
      }))
      .mockResolvedValueOnce(undefined);
    const { unmount } = render(<BrowserViewer browserId={browserId} serverId={serverId} />);
    await waitFor(() => expect(viewerMocks.subscribeMetadata).toHaveBeenCalledOnce());

    vi.useFakeTimers();
    await act(async () => {
      rejectFirstAttempt?.(new Error("metadata unavailable"));
      await Promise.resolve();
    });
    expect(viewerMocks.metadataClose).toHaveBeenCalledOnce();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(viewerMocks.subscribeMetadata).toHaveBeenCalledTimes(2);

    unmount();
    vi.useRealTimers();
  });

  describe("mobile keyboard forwarding", () => {
    it("maps printable keys and function keys to X11 keysyms", () => {
      expect(browserViewerKeyToKeysym("a")).toBe(0x61);
      expect(browserViewerKeyToKeysym("A")).toBe(0x41);
      expect(browserViewerKeyToKeysym("中")).toBe(0x01000000 | "中".codePointAt(0)!);
      expect(browserViewerKeyToKeysym(" ")).toBe(0x20);
      expect(browserViewerKeyToKeysym("Enter")).toBe(0xff0d);
      expect(browserViewerKeyToKeysym("Backspace")).toBe(0xff08);
      expect(browserViewerKeyToKeysym("ArrowLeft")).toBe(0xff51);
      expect(browserViewerKeyToKeysym("ShiftLeft")).toBe(0xffe1);
      expect(browserViewerKeyToKeysym("F1")).toBe(0xffbe);
      expect(browserViewerKeyToKeysym("F12")).toBe(0xffc9);
      expect(browserViewerKeyToKeysym("Meta")).toBeUndefined();
    });

    it("forwards keydown and keyup to RFB and reports handling", () => {
      const rfb = new EventTarget() as never;
      const sendKey = vi.fn();
      (rfb as unknown as { sendKey: typeof sendKey }).sendKey = sendKey;

      const handledDown = forwardBrowserViewerKey(
        rfb as never,
        true,
        { key: "a", code: "KeyA", isComposing: false },
      );
      expect(handledDown).toBe(true);
      expect(sendKey).toHaveBeenLastCalledWith(0x61, "KeyA", true);

      const handledUp = forwardBrowserViewerKey(
        rfb as never,
        false,
        { key: "a", code: "KeyA", isComposing: false },
      );
      expect(handledUp).toBe(true);
      expect(sendKey).toHaveBeenLastCalledWith(0x61, "KeyA", false);
    });

    it("skips composing events so the IME can finish", () => {
      const rfb = new EventTarget() as never;
      const sendKey = vi.fn();
      (rfb as unknown as { sendKey: typeof sendKey }).sendKey = sendKey;

      const handled = forwardBrowserViewerKey(
        rfb as never,
        true,
        { key: "a", code: "KeyA", isComposing: true },
      );
      expect(handled).toBe(false);
      expect(sendKey).not.toHaveBeenCalled();
    });

    it("forwards composition text as key press pairs", () => {
      const rfb = new EventTarget() as never;
      const sendKey = vi.fn();
      (rfb as unknown as { sendKey: typeof sendKey }).sendKey = sendKey;

      forwardBrowserViewerComposition(rfb as never, "你好");
      expect(sendKey).toHaveBeenCalledTimes(4);
      expect(sendKey).toHaveBeenNthCalledWith(1, 0x01000000 | "你".codePointAt(0)!, "", true);
      expect(sendKey).toHaveBeenNthCalledWith(2, 0x01000000 | "你".codePointAt(0)!, "", false);
      expect(sendKey).toHaveBeenNthCalledWith(3, 0x01000000 | "好".codePointAt(0)!, "", true);
      expect(sendKey).toHaveBeenNthCalledWith(4, 0x01000000 | "好".codePointAt(0)!, "", false);
    });

    it("keeps the keyboard input alive when RFB replaces canvas children", async () => {
      const { unmount } = render(<BrowserViewer browserId={browserId} serverId={serverId} />);
      await waitFor(() => expect(viewerMocks.instances).toHaveLength(1));

      const canvas = document.querySelector(".browser-viewer-canvas");
      const input = document.querySelector<HTMLInputElement>(".browser-viewer-keyboard-input");
      expect(canvas).not.toBeNull();
      expect(input).not.toBeNull();
      // input 必须在 canvas 容器之外：RFB 连接时对容器 replaceChildren()，
      // 容器内的 input 会被移除且 React 不知情（ref 指向 detached 节点）。
      expect(canvas!.contains(input)).toBe(false);

      // 模拟 noVNC 连接时清空容器（真实 RFB 构造时执行 replaceChildren）。
      canvas!.replaceChildren();
      const rfb = viewerMocks.instances[0] as unknown as EventTarget;
      rfb.dispatchEvent(new Event("connect"));
      const keyboardButton = screen.getByRole("button", { name: "Show keyboard" });
      await waitFor(() => expect(keyboardButton).toBeEnabled());

      // 点击画面不自动唤起键盘（画面点击是鼠标语义）。
      fireEvent.click(canvas!);
      expect(document.activeElement).not.toBe(input);

      // 点击键盘按钮打开软键盘。
      fireEvent.click(keyboardButton);
      expect(document.activeElement).toBe(input);
      expect(input!.readOnly).toBe(false);
      expect(document.querySelector(".browser-viewer-keyboard-input")).not.toBeNull();

      // 再点收起软键盘（readonly + blur）。
      fireEvent.click(keyboardButton);
      expect(input!.readOnly).toBe(true);

      unmount();
    });
  });
});
