import { describe, expect, it } from "vitest";
import { parseBrowserViewerRoute } from "../browser-viewer-route";

const browserId = "11111111-2222-4333-8444-555555555555";
const serverId = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

describe("parseBrowserViewerRoute", () => {
  it("accepts one exact browser route with a paired server id", () => {
    expect(parseBrowserViewerRoute(`/browser/${browserId}`, `?server_id=${serverId}`)).toEqual({
      browserId,
      serverId,
    });
  });

  it.each([
    ["/", ""],
    [`/browser/${browserId}/extra`, `?server_id=${serverId}`],
    ["/browser/%", `?server_id=${serverId}`],
    [`/browser/${browserId}`, ""],
    [`/browser/not-a-uuid`, `?server_id=${serverId}`],
  ])("rejects an invalid viewer location %s", (pathname, search) => {
    expect(parseBrowserViewerRoute(pathname, search)).toBeUndefined();
  });
});
