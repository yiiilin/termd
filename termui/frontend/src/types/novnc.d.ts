declare module "@novnc/novnc" {
  export interface RFBOptions {
    credentials?: Record<string, string>;
    shared?: boolean;
    repeaterID?: string;
    wsProtocols?: string[];
  }

  export interface RFBDisconnectEvent extends Event {
    detail?: { clean?: boolean };
  }

  export default class RFB extends EventTarget {
    constructor(target: HTMLElement, urlOrChannel: string | WebSocket, options?: RFBOptions);
    background: string;
    clipViewport: boolean;
    compressionLevel: number;
    dragViewport: boolean;
    focusOnClick: boolean;
    qualityLevel: number;
    resizeSession: boolean;
    scaleViewport: boolean;
    viewOnly: boolean;
    disconnect(): void;
    focus(options?: FocusOptions): void;
    sendCtrlAltDel(): void;
  }
}
