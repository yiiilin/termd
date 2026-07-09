import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ConnectionPanel, ConnectionStatusPanel } from "../components/ConnectionPanel";
import { DaemonManagerPanel } from "../components/DaemonManagerPanel";
import type { PairedServerState } from "../protocol/types";

const relayUrlWithSecret = "wss://relay.example/termd/ws?relay_token=relay-secret#debug";
const relayUrlForDisplay = "wss://relay.example/termd/ws";

const server: PairedServerState = {
  server_id: "00000000-0000-0000-0000-000000000501",
  daemon_public_key: "ed25519-v1:daemon-public",
  url: relayUrlWithSecret,
  paired_at_ms: 1710000000000,
};

describe("连接面板", () => {
  it("pairing token 输入框使用 password 类型", () => {
    render(
      <ConnectionPanel
        url="ws://127.0.0.1:8765/ws"
        token="secret-token"
        status="idle"
        onUrlChange={vi.fn()}
        onTokenChange={vi.fn()}
        onPair={vi.fn()}
      />,
    );

    expect(screen.getByLabelText("Pairing token")).toHaveAttribute("type", "password");
  });

  it("展示态 URL 去掉 query 和 fragment", () => {
    render(
      <>
        <ConnectionPanel
          url={relayUrlWithSecret}
          token=""
          status="idle"
          onUrlChange={vi.fn()}
          onTokenChange={vi.fn()}
          onPair={vi.fn()}
        />
        <ConnectionStatusPanel url={relayUrlWithSecret} status="idle" />
        <DaemonManagerPanel
          servers={[{ server, label: "Relay daemon" }]}
          activeServerId={server.server_id}
          renameDraft=""
          onSelect={vi.fn()}
          onStartRename={vi.fn()}
          onRenameDraftChange={vi.fn()}
          onSaveRename={vi.fn()}
          onCancelRename={vi.fn()}
          onForget={vi.fn()}
        />
      </>,
    );

    expect(screen.getAllByText(relayUrlForDisplay)).toHaveLength(3);
    expect(document.body.textContent).not.toContain("relay-secret");
    expect(document.body.textContent).not.toContain("relay_token");
  });
});
