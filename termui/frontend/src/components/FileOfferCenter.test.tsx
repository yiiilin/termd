import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { I18nProvider } from "../i18n";
import type { FileOfferPayload } from "../protocol/types";
import { FileOfferCenter } from "./FileOfferCenter";

const older: FileOfferPayload = {
  offer_id: "00000000-0000-4000-8000-000000000601",
  name: "untrusted-name.txt",
  path: "/canonical/archive/report.zip",
  size_bytes: 1234,
  created_at_ms: 1785144000000,
  expires_at_ms: 1785230400000,
};

const newer: FileOfferPayload = {
  ...older,
  offer_id: "00000000-0000-4000-8000-000000000602",
  path: "/canonical/newer.tar",
  size_bytes: 2048,
  created_at_ms: older.created_at_ms + 1,
};

describe("FileOfferCenter", () => {
  it("shows canonical basenames and paths newest first", () => {
    render(
      <I18nProvider locale="en-US">
        <FileOfferCenter
          offers={[newer, older]}
          onDownload={vi.fn()}
          onDismiss={vi.fn()}
        />
      </I18nProvider>,
    );

    const alerts = screen.getAllByRole("article");
    expect(alerts).toHaveLength(2);
    expect(within(alerts[0]).getByText("newer.tar")).toBeInTheDocument();
    expect(within(alerts[0]).getByText("/canonical/newer.tar")).toBeInTheDocument();
    expect(within(alerts[0]).getByText("2 KB")).toBeInTheDocument();
    expect(within(alerts[1]).getByText("report.zip")).toBeInTheDocument();
    expect(screen.queryByText("untrusted-name.txt")).not.toBeInTheDocument();
  });

  it("keeps download and dismiss actions independent and exposes errors", async () => {
    const user = userEvent.setup();
    const onDownload = vi.fn();
    const onDismiss = vi.fn();
    render(
      <I18nProvider locale="en-US">
        <FileOfferCenter
          offers={[{ ...newer, error: "Download unavailable" }, older]}
          onDownload={onDownload}
          onDismiss={onDismiss}
        />
      </I18nProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Download newer.tar" }));
    await user.click(screen.getByRole("button", { name: "Dismiss report.zip" }));

    expect(onDownload).toHaveBeenCalledWith(newer.offer_id);
    expect(onDismiss).toHaveBeenCalledWith(older.offer_id);
    expect(screen.getByRole("alert")).toHaveTextContent("Download unavailable");
  });

  it("renders permission onboarding as an explicit user action", async () => {
    const user = userEvent.setup();
    const onRequestNotificationPermission = vi.fn();
    render(
      <I18nProvider locale="en-US">
        <FileOfferCenter
          offers={[]}
          showNotificationPermissionPrompt
          onDownload={vi.fn()}
          onDismiss={vi.fn()}
          onRequestNotificationPermission={onRequestNotificationPermission}
          onDismissNotificationPermissionPrompt={vi.fn()}
        />
      </I18nProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Enable notifications" }));
    expect(onRequestNotificationPermission).toHaveBeenCalledTimes(1);
  });
});
