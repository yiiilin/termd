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
  onMetadata?: (data: unknown) => void;
  onTerminal?: (data: unknown) => void;

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
    this.metadata?.close();
    this.metadata = undefined;
    this.metadataOpen = undefined;
    return this.connectMetadata();
  }

  async openTerminal(command: WorkspaceCommand): Promise<WebSocket> {
    this.terminal?.close();
    const socket = await this.open("terminal", (data) => this.onTerminal?.(data));
    this.terminal = socket;
    socket.send(JSON.stringify(command));
    return socket;
  }

  sendTerminal(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    if (!this.terminal || this.terminal.readyState !== 1) {
      throw new Error("terminal websocket is not open");
    }
    this.terminal.send(data);
  }

  closeTerminal(): void {
    this.terminal?.close();
    this.terminal = undefined;
  }

  close(): void {
    this.closeTerminal();
    this.metadataGeneration += 1;
    this.metadata?.close();
    this.metadata = undefined;
    this.metadataOpen = undefined;
  }

  private async open(kind: "metadata" | "terminal", receive: (data: unknown) => void): Promise<WebSocket> {
    const token = await this.tokens.get();
    const socket = new WebSocket(workspaceWebSocketUrl(this.serverUrl, kind), ["termd.v0.7", token]);
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) => receive(event.data);
    await new Promise<void>((resolve, reject) => {
      socket.onopen = () => resolve();
      socket.onerror = () => reject(new Error(`${kind} websocket failed to open`));
      socket.onclose = () => reject(new Error(`${kind} websocket closed while opening`));
    });
    socket.onclose = null;
    return socket;
  }
}
