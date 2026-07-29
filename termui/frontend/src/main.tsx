import React, { lazy, Suspense } from "react";
import { createRoot } from "react-dom/client";
import "@xterm/xterm/css/xterm.css";
import App from "./App";
import { parseBrowserViewerRoute } from "./browser-viewer-route";
import { registerTermdServiceWorker } from "./pwa";
import "./styles.css";

const BrowserViewer = lazy(() => import("./components/BrowserViewer"));
const browserRoute = parseBrowserViewerRoute(window.location.pathname, window.location.search);

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    {browserRoute ? (
      <Suspense fallback={<div className="browser-viewer-boot" aria-hidden="true" />}>
        <BrowserViewer browserId={browserRoute.browserId} serverId={browserRoute.serverId} />
      </Suspense>
    ) : <App />}
  </React.StrictMode>,
);

registerTermdServiceWorker();
