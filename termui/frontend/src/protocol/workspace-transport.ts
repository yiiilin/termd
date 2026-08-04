import { ProtocolClientError } from "./errors";
import { recordTermdDiagnostic } from "../diagnostics";

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
  type: "terminal.create" | "terminal.create_in_session_cwd" | "terminal.attach";
  payload: unknown;
}

interface SocketDiagnosticContext {
  connectionId: string;
  kind: "metadata" | "terminal";
  createdAtMs: number;
  generation?: number;
  commandType?: WorkspaceCommand["type"];
  sessionId?: string;
}

let nextWorkspaceTransportDiagnosticId = 0;

function terminalCommandSessionId(command: WorkspaceCommand): string | undefined {
  if (!command.payload || typeof command.payload !== "object") return undefined;
  const sessionId = (command.payload as { session_id?: unknown }).session_id;
  return typeof sessionId === "string" ? sessionId : undefined;
}

export class WorkspaceTransport {
  private readonly diagnosticId = `transport-${++nextWorkspaceTransportDiagnosticId}`;
  private nextSocketDiagnosticId = 0;
  private readonly socketDiagnostics = new WeakMap<WebSocket, SocketDiagnosticContext>();
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
    private readonly diagnosticOwnerId?: string,
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
          this.requestSocketClose(socket, "metadata_superseded_after_open");
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
    if (socket) this.requestSocketClose(socket, "metadata_reconnect");
    return this.connectMetadata();
  }

  sendMetadata(data: string): void {
    if (!this.metadata || this.metadata.readyState !== 1) {
      throw new ProtocolClientError("connection_closed", "metadata websocket is not open");
    }
    this.metadata.send(data);
  }

  async openTerminal(command: WorkspaceCommand): Promise<WebSocket> {
    this.closeTerminal(`replace_for_${command.type}`);
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
            this.requestSocketClose(opening, "terminal_superseded_on_create");
            return;
          }
          this.terminalOpening = opening;
        },
        {
          generation,
          commandType: command.type,
          sessionId: terminalCommandSessionId(command),
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
      this.requestSocketClose(socket, "terminal_superseded_after_open");
      throw new ProtocolClientError("stale_connection", "terminal websocket was superseded");
    }
    this.terminal = socket;
    socket.onclose = () => {
      if (generation !== this.terminalGeneration || this.terminal !== socket) return;
      this.terminal = undefined;
      this.onTerminalClose?.();
    };
    socket.send(JSON.stringify(command));
    this.recordSocketDiagnostic("terminal_command_sent", socket);
    return socket;
  }

  sendTerminal(data: string | ArrayBufferLike | Blob | ArrayBufferView): void {
    if (!this.terminal || this.terminal.readyState !== 1) {
      throw new ProtocolClientError("connection_closed", "terminal websocket is not open");
    }
    this.terminal.send(data);
  }

  closeTerminal(reason = "terminal_close_requested"): void {
    this.terminalGeneration += 1;
    const socket = this.terminal;
    const opening = this.terminalOpening;
    this.terminal = undefined;
    this.terminalOpening = undefined;
    if (opening) this.requestSocketClose(opening, reason, true);
    if (socket === opening) return;
    if (socket) this.requestSocketClose(socket, reason, true);
  }

  close(reason = "transport_close_requested"): void {
    this.closeTerminal(reason);
    this.metadataGeneration += 1;
    const socket = this.metadata;
    this.metadata = undefined;
    this.metadataOpen = undefined;
    if (socket) this.requestSocketClose(socket, reason);
  }

  private async open(
    kind: "metadata" | "terminal",
    receive: (data: unknown, socket: WebSocket) => void,
    onCreated?: (socket: WebSocket) => void,
    diagnosticFields: Partial<Pick<SocketDiagnosticContext, "generation" | "commandType" | "sessionId">> = {},
  ): Promise<WebSocket> {
    const token = await this.tokens.get();
    const socket = new WebSocket(workspaceWebSocketUrl(this.serverUrl, kind), ["termd.v0.7", token]);
    const context: SocketDiagnosticContext = {
      connectionId: `${this.diagnosticId}-socket-${++this.nextSocketDiagnosticId}`,
      kind,
      createdAtMs: Date.now(),
      ...diagnosticFields,
    };
    this.socketDiagnostics.set(socket, context);
    this.recordSocketDiagnostic(`${kind}_socket_created`, socket);
    socket.addEventListener("open", () => {
      this.recordSocketDiagnostic(`${kind}_socket_opened`, socket);
    });
    socket.addEventListener("error", () => {
      this.recordSocketDiagnostic(`${kind}_socket_error`, socket, {
        readyState: socket.readyState,
      });
    });
    socket.addEventListener("close", (event) => {
      this.recordSocketDiagnostic(`${kind}_socket_closed`, socket, {
        code: event.code,
        reason: event.reason || undefined,
        wasClean: event.wasClean,
        lifetimeMs: Math.max(0, Date.now() - context.createdAtMs),
      });
    });
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

  private requestSocketClose(socket: WebSocket, reason: string, stack = false): void {
    this.recordSocketDiagnostic(`${this.socketDiagnostics.get(socket)?.kind ?? "workspace"}_socket_close_requested`, socket, {
      reason,
      readyState: socket.readyState,
    }, stack);
    socket.close();
  }

  private recordSocketDiagnostic(
    name: string,
    socket: WebSocket,
    fields: Record<string, unknown> = {},
    stack = false,
  ): void {
    const context = this.socketDiagnostics.get(socket);
    recordTermdDiagnostic(name, {
      transportId: this.diagnosticId,
      ownerId: this.diagnosticOwnerId,
      connectionId: context?.connectionId,
      generation: context?.generation,
      commandType: context?.commandType,
      sessionId: context?.sessionId,
      visibilityState: typeof document === "undefined" ? undefined : document.visibilityState,
      browserOnline: typeof navigator === "undefined" ? undefined : navigator.onLine,
      ...fields,
    }, {
      console: context?.kind === "terminal",
      stack,
    });
  }
}
