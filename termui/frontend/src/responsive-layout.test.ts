import { describe, expect, it } from "vitest";
import { panelDefaultsForBand, viewportBandForWidth } from "./responsive-layout";

describe("responsive workspace layout", () => {
  it.each([
    [1101, "wide"],
    [1100, "medium"],
    [901, "medium"],
    [900, "compact"],
    [761, "compact"],
    [760, "mobile"],
  ] as const)("maps %ipx to the %s viewport band", (width, expected) => {
    expect(viewportBandForWidth(width)).toBe(expected);
  });

  it("keeps the terminal dominant in every constrained band", () => {
    expect(panelDefaultsForBand("wide")).toEqual({ sidebarCollapsed: false, filesPanelOpen: true });
    expect(panelDefaultsForBand("medium")).toEqual({ sidebarCollapsed: false, filesPanelOpen: false });
    expect(panelDefaultsForBand("compact")).toEqual({ sidebarCollapsed: true, filesPanelOpen: false });
    expect(panelDefaultsForBand("mobile")).toEqual({ sidebarCollapsed: true, filesPanelOpen: false });
  });
});
