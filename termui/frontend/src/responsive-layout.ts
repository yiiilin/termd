export type ViewportBand = "wide" | "medium" | "compact" | "mobile";

export interface WorkspacePanelDefaults {
  sidebarCollapsed: boolean;
  filesPanelOpen: boolean;
}

export function viewportBandForWidth(width: number): ViewportBand {
  if (width <= 760) {
    return "mobile";
  }
  if (width <= 900) {
    return "compact";
  }
  if (width <= 1100) {
    return "medium";
  }
  return "wide";
}

export function panelDefaultsForBand(band: ViewportBand): WorkspacePanelDefaults {
  switch (band) {
    case "wide":
      return { sidebarCollapsed: false, filesPanelOpen: true };
    case "medium":
      return { sidebarCollapsed: false, filesPanelOpen: false };
    case "compact":
    case "mobile":
      return { sidebarCollapsed: true, filesPanelOpen: false };
  }
}
