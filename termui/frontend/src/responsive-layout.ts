export type ViewportBand = "wide" | "medium" | "compact" | "mobile";

const MOBILE_MAX_WIDTH = 760;
const COMPACT_MAX_WIDTH = 900;
const WIDE_MIN_WIDTH = 1280;

export interface WorkspacePanelDefaults {
  sidebarCollapsed: boolean;
  filesPanelOpen: boolean;
}

export function viewportBandForWidth(width: number): ViewportBand {
  if (width <= MOBILE_MAX_WIDTH) {
    return "mobile";
  }
  if (width <= COMPACT_MAX_WIDTH) {
    return "compact";
  }
  // 在此宽度以下同时展开侧栏和文件面板会过早挤压终端。
  if (width < WIDE_MIN_WIDTH) {
    return "medium";
  }
  return "wide";
}

export function panelDefaultsForBand(band: ViewportBand): WorkspacePanelDefaults {
  switch (band) {
    case "wide":
      return { sidebarCollapsed: false, filesPanelOpen: true };
    case "medium":
      return { sidebarCollapsed: true, filesPanelOpen: false };
    case "compact":
    case "mobile":
      return { sidebarCollapsed: true, filesPanelOpen: false };
  }
}
