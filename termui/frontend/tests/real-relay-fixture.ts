import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { request } from "node:http";
import { connect, createServer, type Socket } from "node:net";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../..");
const CARGO_MANIFEST = path.join(REPO_ROOT, "Cargo.toml");

export interface StartedProcess {
  child: ChildProcessWithoutNullStreams;
  log: string[];
}

export interface RealRelayFixture {
  token: string;
  relayClientUrl: string;
  relayWebUrl: string;
  serverId: string;
  daemonPublicKey: string;
  diagnostics: () => string;
  issuePairingToken: () => Promise<string>;
  interruptRelayMux: () => Promise<void>;
  restartDaemon: () => Promise<void>;
  waitForRelayReady: () => Promise<void>;
  stop: () => Promise<void>;
}

interface RealRelayFixtureOptions {
  daemonToRelayLatencyMs?: number;
  relayToDaemonLatencyMs?: number;
  daemonToRelayJitterMs?: number;
  relayToDaemonJitterMs?: number;
  daemonToRelayBytesPerSecond?: number;
  relayToDaemonBytesPerSecond?: number;
  blackoutAfterMs?: number;
  blackoutDurationMs?: number;
  enableRelayInterrupt?: boolean;
  enableHttpTunnel?: boolean;
  daemonEnv?: Record<string, string>;
}

interface LatencyProxy {
  listenPort: number;
  diagnostics: () => string;
  interruptConnections: () => Promise<void>;
  stop: () => Promise<void>;
}

interface LinkRules {
  latencyMs: number;
  jitterMs: number;
  bytesPerSecond?: number;
  blackoutAfterMs?: number;
  blackoutDurationMs?: number;
}

export async function startRealRelayFixture(options: RealRelayFixtureOptions = {}): Promise<RealRelayFixture> {
  const termdPort = await pickFreePort();
  const termdHttp = `http://127.0.0.1:${termdPort}`;
  const relayPort = await pickFreePort();
  const relayAddr = `127.0.0.1:${relayPort}`;
  let relayForDaemon = relayAddr;
  let latencyProxy: LatencyProxy | undefined;
  // Unix domain socket 路径有 SUN_LEN 限制；termd 会从 cwd 派生 supervisor socket 目录。
  // 真实 relay 测试必须使用短 cwd，否则创建 PTY 时会因为 socket path 过长失败。
  const tempDir = await mkdtemp(path.join(tmpdir(), "td-"));
  const setupToken = testSecret("relay-setup", relayPort);
  const daemonToken = testSecret("daemon", termdPort);
  const setupTokenFile = path.join(tempDir, "relay-setup-token");
  const daemonRegistryFile = path.join(tempDir, "daemon-registry.json");
  await writeFile(setupTokenFile, `${setupToken}\n`, { mode: 0o600 });
  await chmod(setupTokenFile, 0o600);

  const relayArgs = [
    "run",
    "-q",
    "--manifest-path",
    CARGO_MANIFEST,
    "-p",
    "termrelay",
    "--",
    "--listen",
    relayAddr,
    "--web",
    "--setup-token-file",
    setupTokenFile,
    "--daemon-registry",
    daemonRegistryFile,
  ];
  if (options.enableHttpTunnel) {
    relayArgs.push("--http-tunnel");
  }
  const relay = spawnCargo(relayArgs, "termrelay", tempDir);
  await waitForPort(relayPort, relay, "termrelay");
  const daemonToRelayLatencyMs = Math.max(0, options.daemonToRelayLatencyMs ?? 0);
  const relayToDaemonLatencyMs = Math.max(0, options.relayToDaemonLatencyMs ?? 0);
  const daemonToRelayJitterMs = Math.max(0, options.daemonToRelayJitterMs ?? 0);
  const relayToDaemonJitterMs = Math.max(0, options.relayToDaemonJitterMs ?? 0);
  const blackoutAfterMs = positiveNumberOrUndefined(options.blackoutAfterMs);
  const blackoutDurationMs = positiveNumberOrUndefined(options.blackoutDurationMs);
  const daemonToRelayBytesPerSecond = positiveNumberOrUndefined(options.daemonToRelayBytesPerSecond);
  const relayToDaemonBytesPerSecond = positiveNumberOrUndefined(options.relayToDaemonBytesPerSecond);
  if (
    daemonToRelayLatencyMs > 0 ||
    relayToDaemonLatencyMs > 0 ||
    daemonToRelayJitterMs > 0 ||
    relayToDaemonJitterMs > 0 ||
    daemonToRelayBytesPerSecond ||
    relayToDaemonBytesPerSecond ||
    (blackoutAfterMs && blackoutDurationMs) ||
    options.enableRelayInterrupt
  ) {
    latencyProxy = await startRelayLatencyProxy(relayPort, {
      daemonToRelay: {
        latencyMs: daemonToRelayLatencyMs,
        jitterMs: daemonToRelayJitterMs,
        bytesPerSecond: daemonToRelayBytesPerSecond,
        blackoutAfterMs,
        blackoutDurationMs,
      },
      relayToDaemon: {
        latencyMs: relayToDaemonLatencyMs,
        jitterMs: relayToDaemonJitterMs,
        bytesPerSecond: relayToDaemonBytesPerSecond,
        blackoutAfterMs,
        blackoutDurationMs,
      },
    });
    relayForDaemon = `127.0.0.1:${latencyProxy.listenPort}`;
  }
  const daemonLogs: string[] = [];
  const daemonArgs = [
    "run",
    "-q",
    "--manifest-path",
    CARGO_MANIFEST,
    "-p",
    "termd",
    "--",
    "--listen",
    `127.0.0.1:${termdPort}`,
    "--relay",
    `ws://${relayForDaemon}`,
    "--relay-daemon-token",
    daemonToken,
  ];
  let daemon = spawnCargo(daemonArgs, "termd", tempDir, options.daemonEnv, daemonLogs);
  await waitForPort(termdPort, daemon, "termd");

  const serverId = await serverIdFromHealthz(termdHttp);
  const relayClientUrl = `ws://${relayAddr}/ws`;
  const relayHttp = `http://${relayAddr}`;
  await registerDaemonWithRelay(`http://${relayAddr}`, serverId, daemonToken, setupToken);
  const pairing = await issuePairingToken(termdHttp);
  let latestRelayDaemonControlConnectionId = await waitForRelayDaemonMux(
    relayHttp,
    [relay, daemon],
  );

  return {
    token: pairing.token,
    relayClientUrl,
    relayWebUrl: `http://${relayAddr}/`,
    serverId,
    daemonPublicKey: pairing.daemonPublicKey,
    diagnostics: () => [
      `daemon_http=${termdHttp}`,
      `relay_ws=${relayClientUrl}`,
      `relay_web=http://${relayAddr}/`,
      `daemon_relay_ws=ws://${relayForDaemon}`,
      latencyProxy?.diagnostics() ?? "",
      `server_id=${serverId}`,
      relay.log.join(""),
      daemon.log.join(""),
    ].join("\n"),
    issuePairingToken: async () => {
      // 中文注释：多客户端测试需要不同设备身份，所以必须让 daemon 再签发一个独立的一次性 token。
      const nextPairing = await issuePairingToken(termdHttp);
      return nextPairing.token;
    },
    interruptRelayMux: async () => {
      if (!latencyProxy) {
        throw new Error("relay mux interrupt requires fixture network proxy");
      }
      await latencyProxy.interruptConnections();
    },
    restartDaemon: async () => {
      // 中文注释：真实 relay 恢复验收需要保留同一个 state 目录，
      // 这样 daemon 重启后才能从 supervisor/socket restore 已持久化 session。
      await stopProcess(daemon, "termd");
      daemon = spawnCargo(daemonArgs, "termd", tempDir, options.daemonEnv, daemonLogs);
      await waitForPort(termdPort, daemon, "termd");
      const restartedServerId = await serverIdFromHealthz(termdHttp);
      if (restartedServerId !== serverId) {
        throw new Error(`daemon restart changed server_id: expected ${serverId}, got ${restartedServerId}`);
      }
      await registerDaemonWithRelay(`http://${relayAddr}`, serverId, daemonToken, setupToken);
      latestRelayDaemonControlConnectionId = await waitForRelayDaemonMux(
        relayHttp,
        [relay, daemon],
        latestRelayDaemonControlConnectionId,
      );
    },
    waitForRelayReady: async () => {
      latestRelayDaemonControlConnectionId = await waitForRelayDaemonMux(
        relayHttp,
        [relay, daemon],
        latestRelayDaemonControlConnectionId,
      );
    },
    stop: async () => {
      await stopProcess(daemon, "termd");
      await latencyProxy?.stop();
      await stopProcess(relay, "termrelay");
      await rm(tempDir, { recursive: true, force: true });
    },
  };
}

async function waitForRelayDaemonMux(
  relayHttp: string,
  processes: StartedProcess[],
  afterConnectionId = 0,
): Promise<number> {
  const deadline = Date.now() + 30_000;
  let lastError = "not probed";
  while (Date.now() < deadline) {
    for (const process of processes) {
      if (process.child.exitCode !== null) {
        throw new Error(`process exited while waiting for relay daemon mux\n${process.log.join("")}`);
      }
    }
    try {
      const health = await relayHealthz(relayHttp);
      // 中文注释：不能用真实 client route 做 readiness probe；pair ticket 是一次性凭证，
      // 探测会提前消费 token，并且会无意义触发 daemon data 反连。
      if (
        health.daemon_controls > 0 &&
        health.latest_daemon_control_connection_id > afterConnectionId
      ) {
        return health.latest_daemon_control_connection_id;
      }
      lastError =
        `daemon_controls=${health.daemon_controls}, ` +
        `latest_daemon_control_connection_id=${health.latest_daemon_control_connection_id}, ` +
        `expected_newer_than=${afterConnectionId}`;
    } catch (caught) {
      lastError = caught instanceof Error ? caught.message : String(caught);
    }
    await sleep(150);
  }
  throw new Error(`relay daemon mux did not become ready: ${lastError}\n${processes.map((process) => process.log.join("")).join("\n")}`);
}

async function relayHealthz(relayHttp: string): Promise<{
  daemon_controls: number;
  latest_daemon_control_connection_id: number;
}> {
  const body = await httpRequest(`${relayHttp}/healthz`, { method: "GET" });
  const parsed = JSON.parse(body) as {
    daemon_controls?: number;
    latest_daemon_control_connection_id?: number;
  };
  return {
    daemon_controls: parsed.daemon_controls ?? 0,
    latest_daemon_control_connection_id: parsed.latest_daemon_control_connection_id ?? 0,
  };
}

async function startRelayLatencyProxy(
  targetPort: number,
  options: { daemonToRelay: LinkRules; relayToDaemon: LinkRules },
): Promise<LatencyProxy> {
  const listenPort = await pickFreePort();
  const log: string[] = [];
  const sockets = new Set<Socket>();
  const timers = new Set<NodeJS.Timeout>();
  const activeInterrupts = new Set<() => void>();
  const server = createServer((daemonSocket) => {
    const connectionStartedAt = Date.now();
    const relaySocket = connect({ host: "127.0.0.1", port: targetPort });
    sockets.add(daemonSocket);
    sockets.add(relaySocket);
    log.push(
      `[latency-proxy] accepted daemon socket; daemon->relay=${formatLinkRules(options.daemonToRelay)} relay->daemon=${formatLinkRules(options.relayToDaemon)}\n`,
    );

    // 中文注释：网络代理必须保持 TCP 字节顺序，只改变两端之间的传输时间和吞吐。
    // 这样测试到的是公网延迟/限速，而不是测试代理自己引入的乱序或协议损坏。
    const cleanupPipes = [
      pipeWithNetworkRules(daemonSocket, relaySocket, options.daemonToRelay, timers, connectionStartedAt),
      pipeWithNetworkRules(relaySocket, daemonSocket, options.relayToDaemon, timers, connectionStartedAt),
    ];

    let closed = false;
    const closeBoth = (error?: Error) => {
      if (closed) {
        return;
      }
      closed = true;
      if (error) {
        log.push(`[latency-proxy] socket error: ${error.message}\n`);
      }
      cleanupPipes.forEach((cleanup) => cleanup());
      daemonSocket.destroy();
      relaySocket.destroy();
      sockets.delete(daemonSocket);
      sockets.delete(relaySocket);
      activeInterrupts.delete(interruptConnection);
    };
    const interruptConnection = () => {
      log.push("[latency-proxy] injecting daemon relay mux disconnect\n");
      closeBoth();
    };
    activeInterrupts.add(interruptConnection);
    daemonSocket.on("error", closeBoth);
    relaySocket.on("error", closeBoth);
    daemonSocket.on("close", () => closeBoth());
    relaySocket.on("close", () => closeBoth());
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(listenPort, "127.0.0.1", () => {
      server.off("error", reject);
      resolve();
    });
  });

  return {
    listenPort,
    diagnostics: () => [`latency_proxy=127.0.0.1:${listenPort}->127.0.0.1:${targetPort}`, ...log].join("\n"),
    interruptConnections: async () => {
      const interrupts = [...activeInterrupts];
      if (interrupts.length === 0) {
        log.push("[latency-proxy] interrupt requested but no active daemon relay socket existed\n");
      }
      interrupts.forEach((interrupt) => interrupt());
      await sleep(50);
    },
    stop: async () => {
      for (const timer of timers) {
        clearTimeout(timer);
      }
      timers.clear();
      for (const socket of sockets) {
        socket.destroy();
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

function pipeWithNetworkRules(
  source: Socket,
  target: Socket,
  rules: LinkRules,
  timers: Set<NodeJS.Timeout>,
  connectionStartedAt: number,
): () => void {
  const delayedChunks: Array<{ dueAt: number; data: Buffer; offset: number }> = [];
  let drainTimer: NodeJS.Timeout | undefined;
  let nextSendAt = 0;

  const scheduleDrain = () => {
    if (drainTimer || delayedChunks.length === 0) {
      return;
    }
    const now = Date.now();
    const delay = Math.max(0, delayedChunks[0].dueAt - now, nextSendAt - now);
    drainTimer = setTimeout(() => {
      const timer = drainTimer;
      drainTimer = undefined;
      if (timer) {
        timers.delete(timer);
      }

      const currentTime = Date.now();
      while (
        delayedChunks.length > 0 &&
        delayedChunks[0].dueAt <= currentTime &&
        nextSendAt <= currentTime
      ) {
        const chunk = delayedChunks[0];
        if (target.destroyed) {
          delayedChunks.length = 0;
          break;
        }
        if (rules.bytesPerSecond) {
          const bytesToWrite = Math.min(16 * 1024, chunk.data.length - chunk.offset);
          target.write(chunk.data.subarray(chunk.offset, chunk.offset + bytesToWrite));
          chunk.offset += bytesToWrite;
          nextSendAt = Date.now() + Math.max(1, Math.ceil((bytesToWrite / rules.bytesPerSecond) * 1000));
          if (chunk.offset >= chunk.data.length) {
            delayedChunks.shift();
          }
        } else {
          target.write(chunk.data);
          delayedChunks.shift();
        }
      }
      scheduleDrain();
    }, delay);
    timers.add(drainTimer);
  };

  source.on("data", (chunk) => {
    if (rules.latencyMs <= 0 && !rules.bytesPerSecond) {
      if (!target.destroyed) {
        target.write(chunk);
      }
      return;
    }
    const rawDueAt = Date.now() + rules.latencyMs + randomJitterMs(rules.jitterMs);
    delayedChunks.push({ dueAt: applyBlackout(rawDueAt, rules, connectionStartedAt), data: Buffer.from(chunk), offset: 0 });
    scheduleDrain();
  });

  return () => {
    delayedChunks.length = 0;
    if (drainTimer) {
      clearTimeout(drainTimer);
      timers.delete(drainTimer);
      drainTimer = undefined;
    }
  };
}

function positiveNumberOrUndefined(value: number | undefined): number | undefined {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    return undefined;
  }
  return value;
}

function formatLinkRules(rules: LinkRules): string {
  const jitter = rules.jitterMs > 0 ? `+jitter${rules.jitterMs}ms` : "";
  const bandwidth = rules.bytesPerSecond ? `,${rules.bytesPerSecond}Bps` : "";
  const blackout =
    rules.blackoutAfterMs && rules.blackoutDurationMs
      ? `,blackout@${rules.blackoutAfterMs}ms/${rules.blackoutDurationMs}ms`
      : "";
  return `${rules.latencyMs}ms${jitter}${bandwidth}${blackout}`;
}

function randomJitterMs(jitterMs: number): number {
  if (jitterMs <= 0) {
    return 0;
  }
  return Math.floor(Math.random() * (jitterMs + 1));
}

function applyBlackout(dueAt: number, rules: LinkRules, connectionStartedAt: number): number {
  if (!rules.blackoutAfterMs || !rules.blackoutDurationMs) {
    return dueAt;
  }
  const blackoutStart = connectionStartedAt + rules.blackoutAfterMs;
  const blackoutEnd = blackoutStart + rules.blackoutDurationMs;
  const now = Date.now();
  if ((now >= blackoutStart && now < blackoutEnd) || (dueAt >= blackoutStart && dueAt < blackoutEnd)) {
    return blackoutEnd;
  }
  return dueAt;
}

async function issuePairingToken(termdHttp: string): Promise<{ token: string; daemonPublicKey: string }> {
  const body = await httpRequest(`${termdHttp}/local/pairing-token`, { method: "POST" });
  const parsed = JSON.parse(body) as { token: string; daemon_public_key: string };
  if (!parsed.token.startsWith("termd-pair-")) {
    throw new Error("termd pair token had unexpected shape");
  }
  if (!parsed.daemon_public_key.startsWith("ed25519-v1:")) {
    throw new Error("termd daemon public key had unexpected shape");
  }
  return { token: parsed.token, daemonPublicKey: parsed.daemon_public_key };
}

async function registerDaemonWithRelay(
  relayHttp: string,
  serverId: string,
  daemonToken: string,
  setupToken: string,
): Promise<void> {
  // 中文注释：0.6.0 的 relay 是可信准入层，测试里的 daemon 也必须先用 setup token
  // 注册 daemon admission token；否则 relay 会拒绝 daemon control/data 路由。
  await httpRequest(`${relayHttp}/api/relay/daemon/register`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-termd-relay-setup-token": setupToken,
    },
    body: JSON.stringify({ server_id: serverId, daemon_token: daemonToken }),
  });
}

async function serverIdFromHealthz(termdHttp: string): Promise<string> {
  const body = await httpRequest(`${termdHttp}/healthz`, { method: "GET" });
  const parsed = JSON.parse(body) as { server_id: string };
  return parsed.server_id;
}

function httpRequest(
  url: string,
  options: { method: "GET" | "POST"; headers?: Record<string, string>; body?: string },
): Promise<string> {
  return new Promise((resolve, reject) => {
    const req = request(url, { method: options.method, headers: options.headers }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (chunk) => {
        body += chunk;
      });
      res.on("end", () => {
        if ((res.statusCode ?? 500) >= 400) {
          reject(new Error(`HTTP ${res.statusCode}`));
          return;
        }
        resolve(body);
      });
    });
    req.on("error", reject);
    if (options.body) {
      req.write(options.body);
    }
    req.end();
  });
}

function testSecret(prefix: string, port: number): string {
  return `termd-playwright-${prefix}-${process.pid}-${port}-${Date.now().toString(36)}`;
}

function spawnCargo(
  args: string[],
  label: string,
  cwd: string,
  extraEnv: Record<string, string> = {},
  sharedLog?: string[],
): StartedProcess {
  const log = sharedLog ?? [];
  // 中文注释：默认保持测试日志简短；排查真实 relay 压力问题时允许单次命令提高 Rust 日志级别。
  const rustLog = process.env.REAL_RELAY_RUST_LOG ?? "termd=info,termrelay=info";
  const child = spawn("cargo", args, {
    cwd,
    detached: true,
    env: { ...process.env, ...extraEnv, RUST_LOG: rustLog },
  });

  child.stdout.on("data", (chunk) => log.push(`[${label}:stdout] ${chunk.toString()}`));
  child.stderr.on("data", (chunk) => log.push(`[${label}:stderr] ${chunk.toString()}`));
  return { child, log };
}

async function waitForPort(port: number, process: StartedProcess, label: string): Promise<void> {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (process.child.exitCode !== null) {
      throw new Error(`${label} exited early\n${process.log.join("")}`);
    }
    if (await canOpenTcpPort(port)) {
      return;
    }
    await sleep(100);
  }
  throw new Error(`${label} did not listen on ${port}\n${process.log.join("")}`);
}

async function canOpenTcpPort(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(false);
    }, 80);
    socket.on("connect", () => {
      clearTimeout(timer);
      socket.destroy();
      resolve(true);
    });
    socket.on("error", () => {
      clearTimeout(timer);
      resolve(false);
    });
  });
}

async function pickFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "object" && address) {
        const port = address.port;
        server.close(() => resolve(port));
      } else {
        server.close(() => reject(new Error("failed to pick free port")));
      }
    });
    server.on("error", reject);
  });
}

export async function stopProcess(startedProcess: StartedProcess, label: string): Promise<void> {
  const processGroupId = startedProcess.child.pid;
  if (processGroupId === undefined || !isProcessGroupAlive(processGroupId)) {
    return;
  }
  process.kill(-processGroupId, "SIGTERM");
  if (await waitForProcessGroupExit(processGroupId, 5_000)) {
    return;
  }
  process.kill(-processGroupId, "SIGKILL");
  if (await waitForProcessGroupExit(processGroupId, 5_000)) {
    return;
  }
  throw new Error(`${label} did not exit after SIGTERM/SIGKILL`);
}

function isProcessGroupAlive(processGroupId: number): boolean {
  try {
    process.kill(-processGroupId, 0);
    return true;
  } catch (caught) {
    if (caught instanceof Error && "code" in caught && caught.code === "ESRCH") {
      return false;
    }
    throw caught;
  }
}

async function waitForProcessGroupExit(processGroupId: number, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!isProcessGroupAlive(processGroupId)) {
      return true;
    }
    await sleep(50);
  }
  return !isProcessGroupAlive(processGroupId);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
