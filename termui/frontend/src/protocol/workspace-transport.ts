import { ProtocolClientError } from "./errors";

function workspaceWebSocketUrl(serverUrl: string, kind: "metadata" | "terminal"): string {
  const parsed = new URL(serverUrl, globalThis.location?.href);
  parsed.protocol = parsed.protocol === "https:" ? "wss:" : parsed.protocol === "http:" ? "ws:" : parsed.protocol;
  parsed.search = "";
  parsed.hash = "";
  parsed.pathname = parsed.pathname.replace(/\/ws(?:\/(?:metadata|terminal))?\/?$/, "") + `/ws/${kind}`;
  return parsed.toString();
}

interface TokenProvider {
  get(): Promise<string>;
}

export interface WorkspaceCommand {
  type: "terminal.create" | "terminal.attach";
  payload: unknown;
}

export class WorkspaceTransport {
  private metadata?: WebSocket;
  private metadataOpen?: Promise<WebSocket>;
  private metadataGeneration = 0;
  private terminal?: WebSocket;
  private terminalOpening?: WebSocket;
  private terminalGeneration = 0;
  onMetadata?: (data: unknown) => void;
  onTerminal?: (data: unknown) => void;
  onMetadataClose?: () => void;
  onTerminalClose?: () => void;

  constructor(
    private readonly serverUrl: string,
    private readonly tokens: TokenProvider,
  ) {}

  async connectMetadata(): Promise<WebSocket> {
    if (this.metadata && this.metadata.readyState < 2) {
      return this.metadata;
    }
    if (this.metadataOpen) {
      return this.metadataOpen;
    }
    const generation = this.metadataGeneration;
    const opening = this.open("metadata", (data) => this.onMetadata?.(data))
      .then((socket) => {
        if (generation !== this.metadataGeneration) {
          socket.close();
          throw new Error("metadata websocket was superseded");
        }
        this.metadata = socket;
        socket.onclose = () => {
          if (this.metadata !== socket) return;
          this.metadata = undefined;
          this.onMetadataClose?.();
        };
        return socket;
      })
      .finally(() => {
        if (this.metadataOpen === opening) {
          this.metadataOpen = undefined;
        }
      });
    this.metadataOpen = opening;
    return opening;
  }

  async reconnectMetadata(): Promise<WebSocket> {
    this.metadataGeneration += 1;
    const socket = this.metadata;
    this.metadata = undefined;
    this.metadataOpen = undefined;
    socket?.close();
    return this.connectMetadata();
  }

  sendMetadata(data: string): void {
    if (!this.metadata || this.metadata.readyState !== 1) {
      throw new ProtocolClientError("connection_closed", "metadata websocket is not open");
    }
    this.metadata.send(data);
  }

  async openTerminal(command: WorkspaceCommand): Promise<WebSocket> {
    this.closeTerminal();
    const generation = this.terminalGeneration;
    let socket: WebSocket;
    try {
      socket = await this.open(
        "terminal",
        (data, source) => {
          if (generation !== this.terminalGeneration) return;
          if (this.terminal !== source && this.terminalOpening !== source) return;
          this.onTerminal?.(data);
        },
        (opening) => {
          if (generation !== this.terminalGeneration) {
            opening.close();
            return;
          }
          this.terminalOpening = opening;
        },
      );
    } catch (caught) {
      if (generation !== this.terminalGeneration) {
        throw new ProtocolClientError("stale_connection", "terminal websocket was superseded");
      }
      this.terminalOpening = undefined;
      throw caught;
    }
    if (this.terminalOpening === socket) this.terminalOpening = undefined;
    if (generation !== this.terminalGeneration) {
      socket.close();
      throw new ProtocolClientError("stale_connection", "terminal websocket was superseded");
    }
    this.terminal = socket;
    socket.onclose = () => {
      if (generation !== this.terminalGeneration || this.terminal !== socket) return;
      this.terminal = undefined;
      this.onTerminalClose?.();
    };
    socket.send(JSON.stringify(command));
    return socket;
  }

  sendTerminal(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    if (!this.terminal || this.terminal.readyState !== 1) {
      throw new ProtocolClientError("connection_closed", "terminal websocket is not open");
    }
    this.terminal.send(data);
  }

  closeTerminal(): void {
    this.terminalGeneration += 1;
    const socket = this.terminal;
    const opening = this.terminalOpening;
    this.terminal = undefined;
    this.terminalOpening = undefined;
    opening?.close();
    if (socket === opening) return;
    socket?.close();
  }

  close(): void {
    this.closeTerminal();
    this.metadataGeneration += 1;
    const socket = this.metadata;
    this.metadata = undefined;
    this.metadataOpen = undefined;
    socket?.close();
  }

  private async open(
    kind: "metadata" | "terminal",
    receive: (data: unknown, socket: WebSocket) => void,
    onCreated?: (socket: WebSocket) => void,
  ): Promise<WebSocket> {
    const token = await this.tokens.get();
    const socket = new WebSocket(workspaceWebSocketUrl(this.serverUrl, kind), ["termd.v0.7", token]);
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) => receive(event.data, socket);
    await new Promise<void>((resolve, reject) => {
      socket.onopen = () => resolve();
      socket.onerror = () => reject(new Error(`${kind} websocket failed to open`));
      socket.onclose = () => reject(new Error(`${kind} websocket closed while opening`));
      onCreated?.(socket);
    });
    socket.onclose = null;
    return socket;
  }
}
