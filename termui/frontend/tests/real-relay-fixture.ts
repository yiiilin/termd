import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { request } from "node:http";
import { connect, createServer } from "node:net";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../..");
const CARGO_MANIFEST = path.join(REPO_ROOT, "Cargo.toml");

interface StartedProcess {
  child: ChildProcessWithoutNullStreams;
  log: string[];
}

export interface RealRelayFixture {
  token: string;
  relayClientUrl: string;
  serverId: string;
  stop: () => Promise<void>;
}

export async function startRealRelayFixture(): Promise<RealRelayFixture> {
  const termdPort = await pickFreePort();
  const termdHttp = `http://127.0.0.1:${termdPort}`;
  const relayPort = await pickFreePort();
  const relayAddr = `127.0.0.1:${relayPort}`;
  const tempDir = path.join(tmpdir(), `termd-web-relay-${Date.now()}-${Math.random().toString(16).slice(2)}`);
  await mkdir(tempDir, { recursive: true });

  const relay = spawnCargo(["run", "-q", "--manifest-path", CARGO_MANIFEST, "-p", "termrelay", "--", "--listen", relayAddr], "termrelay", tempDir);
  await waitForPort(relayPort, relay, "termrelay");
  const daemon = spawnCargo(
    [
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
      `ws://${relayAddr}`,
    ],
    "termd",
    tempDir,
  );
  await waitForPort(termdPort, daemon, "termd");

  const token = await issueToken(termdHttp);
  const serverId = await serverIdFromHealthz(termdHttp);
  const relayClientUrl = `ws://${relayAddr}/ws`;

  return {
    token,
    relayClientUrl,
    serverId,
    stop: async () => {
      stopProcess(daemon);
      stopProcess(relay);
      await rm(tempDir, { recursive: true, force: true });
    },
  };
}

async function issueToken(termdHttp: string): Promise<string> {
  const body = await httpRequest(`${termdHttp}/local/pairing-token`, { method: "POST" });
  const parsed = JSON.parse(body) as { token: string };
  if (!parsed.token.startsWith("termd-pair-")) {
    throw new Error("termd pair token had unexpected shape");
  }
  return parsed.token;
}

async function serverIdFromHealthz(termdHttp: string): Promise<string> {
  const body = await httpRequest(`${termdHttp}/healthz`, { method: "GET" });
  const parsed = JSON.parse(body) as { server_id: string };
  return parsed.server_id;
}

function httpRequest(url: string, options: { method: "GET" | "POST" }): Promise<string> {
  return new Promise((resolve, reject) => {
    const req = request(url, { method: options.method }, (res) => {
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
    req.end();
  });
}

function spawnCargo(args: string[], label: string, cwd: string): StartedProcess {
  const log: string[] = [];
  const child = spawn("cargo", args, {
    cwd,
    env: { ...process.env, RUST_LOG: "termd=info,termrelay=info" },
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

function stopProcess(process: StartedProcess): void {
  if (process.child.exitCode === null) {
    process.child.kill();
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
