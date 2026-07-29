import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { I18nProvider } from "../i18n";
import type { BrowserSession } from "../protocol/browser-client";
import type { DeviceState, PairedServerState } from "../protocol/types";
import { BrowserWorkspaceDialog } from "./BrowserWorkspaceDialog";

const clientMocks = vi.hoisted(() => ({
  list: vi.fn<() => Promise<BrowserSession[]>>(),
  create: vi.fn(),
  close: vi.fn(),
  dispose: vi.fn(),
}));

vi.mock("../protocol/browser-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../protocol/browser-client")>();
  return {
    ...actual,
    BrowserWorkspaceClient: class {
      list = clientMocks.list;
      create = clientMocks.create;
      close = clientMocks.close;
      dispose = clientMocks.dispose;
    },
  };
});

const server: PairedServerState = {
  server_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  daemon_public_key: "daemon-key",
  device_certificate: "certificate",
  url: "wss://relay.example/ws",
  paired_at_ms: 1,
};

const device: DeviceState = {
  device_id: "11111111-2222-4333-8444-555555555555",
  device_public_key: "device-key",
  device_signing_key_secret: "device-secret",
};

const session: BrowserSession = {
  browser_id: "99999999-8888-4777-8666-555555555555",
  state: "running",
  display_url: "https://intranet.example/",
  width: 1440,
  height: 900,
  created_at_ms: Date.UTC(2026, 6, 28, 5, 0, 0),
};

function renderDialog() {
  return render(
    <I18nProvider locale="en-US">
      <BrowserWorkspaceDialog open server={server} device={device} onClose={vi.fn()} />
    </I18nProvider>,
  );
}

describe("BrowserWorkspaceDialog", () => {
  beforeEach(() => {
    clientMocks.list.mockResolvedValue([session]);
    clientMocks.create.mockResolvedValue(session);
    clientMocks.close.mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.restoreAllMocks();
    for (const mock of Object.values(clientMocks)) {
      mock.mockClear();
    }
  });

  it("lists persistent browser sessions and opens one in an independent window", async () => {
    const user = userEvent.setup();
    const open = vi.spyOn(window, "open").mockReturnValue({ opener: window } as Window);
    renderDialog();

    await screen.findByText(session.display_url);
    await user.click(screen.getByRole("button", { name: `Open ${session.display_url}` }));

    expect(open).toHaveBeenCalledWith(
      `/browser/${session.browser_id}?server_id=${server.server_id}`,
      "_blank",
    );
  });

  it("reserves the iPhone-compatible popup before awaiting browser creation", async () => {
    const user = userEvent.setup();
    const replace = vi.fn();
    const popup = {
      closed: false,
      close: vi.fn(),
      opener: window,
      location: { replace },
      document: {
        title: "",
        body: { textContent: "", style: { cssText: "" } },
      },
    } as unknown as Window;
    const open = vi.spyOn(window, "open").mockReturnValue(popup);
    renderDialog();

    const url = screen.getByRole("textbox", { name: "Address" });
    await user.clear(url);
    await user.type(url, "https://intranet.example");
    await user.click(screen.getByRole("button", { name: "Open browser" }));

    expect(open).toHaveBeenCalledWith("about:blank", "_blank");
    await waitFor(() => expect(clientMocks.create).toHaveBeenCalledWith({
      url: "https://intranet.example",
      width: 1440,
      height: 900,
    }));
    expect(replace).toHaveBeenCalledWith(
      `/browser/${session.browser_id}?server_id=${server.server_id}`,
    );
  });

  it("requires confirmation before stopping a browser session", async () => {
    const user = userEvent.setup();
    renderDialog();

    await screen.findByText(session.display_url);
    await user.click(screen.getByRole("button", { name: `Stop ${session.display_url}` }));
    expect(screen.getByRole("alertdialog", { name: "Stop browser session?" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Stop browser" }));

    await waitFor(() => expect(clientMocks.close).toHaveBeenCalledWith(session.browser_id));
    expect(screen.queryByText(session.display_url)).not.toBeInTheDocument();
  });
});
