import { useState } from "react";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PairingQrScanner } from "./PairingQrScanner";

const scannerMock = vi.hoisted(() => ({
  destroy: vi.fn(),
  hasCamera: vi.fn<() => Promise<boolean>>(),
  onDecode: undefined as ((result: { data: string }) => void) | undefined,
  scanImage: vi.fn<() => Promise<{ data: string }>>(),
  start: vi.fn<() => Promise<void>>(),
  stop: vi.fn(),
}));

vi.mock("qr-scanner", () => {
  class MockQrScanner {
    static NO_QR_CODE_FOUND = "No QR code found";
    static hasCamera = scannerMock.hasCamera;
    static scanImage = scannerMock.scanImage;

    constructor(_video: HTMLVideoElement, onDecode: (result: { data: string }) => void) {
      scannerMock.onDecode = onDecode;
    }

    start = scannerMock.start;
    stop = scannerMock.stop;
    destroy = scannerMock.destroy;
  }

  return { default: MockQrScanner };
});

function ScannerHarness() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>Open scanner</button>
      {open ? <PairingQrScanner onDetected={() => undefined} onClose={() => setOpen(false)} /> : null}
    </>
  );
}

describe("PairingQrScanner dialog", () => {
  beforeEach(() => {
    scannerMock.destroy.mockClear();
    scannerMock.hasCamera.mockReset().mockResolvedValue(true);
    scannerMock.onDecode = undefined;
    scannerMock.scanImage.mockReset();
    scannerMock.start.mockReset().mockResolvedValue();
    scannerMock.stop.mockClear();
  });

  it("traps focus and closes with Escape while releasing the camera", async () => {
    const user = userEvent.setup();
    render(<ScannerHarness />);

    const opener = screen.getByRole("button", { name: "Open scanner" });
    await user.click(opener);
    const dialog = await screen.findByRole("dialog", { name: "Scan pairing QR" });
    const close = within(dialog).getByRole("button", { name: "Close scanner" });
    expect(close).toHaveFocus();
    await user.tab({ shift: true });
    expect(within(dialog).getByRole("button", { name: "Upload image" })).toHaveFocus();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull();
    expect(opener).toHaveFocus();
    await waitFor(() => expect(scannerMock.destroy).toHaveBeenCalledTimes(1));
  });

  it("closes from the backdrop and uses the same camera cleanup path", async () => {
    const user = userEvent.setup();
    render(<ScannerHarness />);

    await user.click(screen.getByRole("button", { name: "Open scanner" }));
    const dialog = await screen.findByRole("dialog", { name: "Scan pairing QR" });
    fireEvent.mouseDown(dialog.parentElement as HTMLElement);

    expect(screen.queryByRole("dialog", { name: "Scan pairing QR" })).toBeNull();
    await waitFor(() => expect(scannerMock.destroy).toHaveBeenCalledTimes(1));
  });

  it("ignores a late image result after close", async () => {
    const user = userEvent.setup();
    let resolveImage: (result: { data: string }) => void = () => undefined;
    const onDetected = vi.fn();
    scannerMock.scanImage.mockReturnValue(new Promise((resolve) => {
      resolveImage = resolve;
    }));
    const { unmount } = render(<PairingQrScanner onDetected={onDetected} onClose={() => undefined} />);
    const fileInput = screen.getByLabelText("Upload QR image");
    const file = new File(["qr"], "invite.png", { type: "image/png" });
    await user.upload(fileInput, file);
    expect(scannerMock.scanImage).toHaveBeenCalledTimes(1);

    unmount();
    resolveImage({ data: "termd-pair:v1:late" });
    await Promise.resolve();

    expect(onDetected).not.toHaveBeenCalled();
  });

  it("accepts only the first result when camera and image decoding race", async () => {
    const user = userEvent.setup();
    const onDetected = vi.fn();
    scannerMock.scanImage.mockResolvedValue({ data: "termd-pair:v1:image" });
    render(<PairingQrScanner onDetected={onDetected} onClose={() => undefined} />);
    const fileInput = screen.getByLabelText("Upload QR image");
    await user.upload(fileInput, new File(["qr"], "invite.png", { type: "image/png" }));
    await waitFor(() => expect(onDetected).toHaveBeenCalledWith("termd-pair:v1:image"));

    scannerMock.onDecode?.({ data: "termd-pair:v1:camera" });

    expect(onDetected).toHaveBeenCalledTimes(1);
    expect(scannerMock.stop).toHaveBeenCalledTimes(1);
  });
});
