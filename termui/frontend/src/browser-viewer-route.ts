import type { UUID } from "./protocol/types";

export interface BrowserViewerRoute {
  browserId: UUID;
  serverId: UUID;
}

const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export function parseBrowserViewerRoute(pathname: string, search: string): BrowserViewerRoute | undefined {
  const match = pathname.match(/^\/browser\/([^/]+)\/?$/);
  if (!match) {
    return undefined;
  }
  let browserId: string;
  try {
    browserId = decodeURIComponent(match[1]);
  } catch {
    return undefined;
  }
  const serverId = new URLSearchParams(search).get("server_id") ?? "";
  if (!UUID_PATTERN.test(browserId) || !UUID_PATTERN.test(serverId)) {
    return undefined;
  }
  return { browserId, serverId };
}
