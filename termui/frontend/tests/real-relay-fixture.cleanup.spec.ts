import { expect, test } from "@playwright/test";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { stopProcess, stopRuntimeProcessPreservingGroup } from "./real-relay-fixture";

test("fixture process cleanup terminates its detached process group", async () => {
  const parent = spawn(
    process.execPath,
    [
      "-e",
      [
        "const { spawn } = require('node:child_process');",
        "const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });",
        "process.stdout.write(String(child.pid) + '\\n');",
        "setInterval(() => {}, 1000);",
      ].join(" "),
    ],
    { detached: true },
  );
  const parentPid = requiredPid(parent);
  const childPid = await readChildPid(parent);

  try {
    expect(isProcessAlive(parentPid)).toBe(true);
    expect(isProcessAlive(childPid)).toBe(true);

    await stopProcess({ child: parent, log: [] }, "controlled process tree");

    await expect.poll(() => isProcessAlive(parentPid), { timeout: 5_000 }).toBe(false);
    await expect.poll(() => isProcessAlive(childPid), { timeout: 5_000 }).toBe(false);
  } finally {
    killProcessGroup(parentPid);
  }
});

test("fixture daemon restart preserves supervisors until final process-group cleanup", async () => {
  const parent = spawn(
    process.execPath,
    [
      "-e",
      [
        "const { spawn } = require('node:child_process');",
        "const runtime = spawn('/bin/sleep', ['60'], { stdio: 'ignore' });",
        "const supervisor = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)', '__session-supervisor'], { stdio: 'ignore' });",
        "process.stdout.write(`${runtime.pid} ${supervisor.pid}\\n`);",
        "setInterval(() => {}, 1000);",
      ].join(" "),
    ],
    { detached: true },
  );
  const parentPid = requiredPid(parent);
  const [runtimePid, supervisorPid] = await readChildPids(parent);

  try {
    await stopRuntimeProcessPreservingGroup({ child: parent, log: [] }, "sleep");

    await expect.poll(() => isProcessAlive(parentPid), { timeout: 5_000 }).toBe(false);
    await expect.poll(() => isProcessAlive(runtimePid), { timeout: 5_000 }).toBe(false);
    expect(isProcessAlive(supervisorPid)).toBe(true);

    await stopProcess({ child: parent, log: [] }, "controlled supervisor group");
    await expect.poll(() => isProcessAlive(supervisorPid), { timeout: 5_000 }).toBe(false);
  } finally {
    killProcessGroup(parentPid);
  }
});

test("fixture daemon restart finds an exec-replaced runtime at the process-group leader", async () => {
  const runtime = spawn(
    "/bin/bash",
    [
      "-c",
      [
        `"${process.execPath}" -e 'setInterval(() => {}, 1000)' __session-supervisor &`,
        "supervisor_pid=$!;",
        "printf '%s\\n' \"$supervisor_pid\";",
        "exec -a termd /bin/sleep 60",
      ].join(" "),
    ],
    { detached: true },
  );
  const runtimePid = requiredPid(runtime);
  const supervisorPid = await readChildPid(runtime);

  try {
    await stopRuntimeProcessPreservingGroup({ child: runtime, log: [] }, "termd");

    await expect.poll(() => isProcessAlive(runtimePid), { timeout: 5_000 }).toBe(false);
    expect(isProcessAlive(supervisorPid)).toBe(true);

    await stopProcess({ child: runtime, log: [] }, "controlled supervisor group");
    await expect.poll(() => isProcessAlive(supervisorPid), { timeout: 5_000 }).toBe(false);
  } finally {
    killProcessGroup(runtimePid);
  }
});

function requiredPid(child: ChildProcessWithoutNullStreams): number {
  if (child.pid === undefined) {
    throw new Error("controlled process did not receive a pid");
  }
  return child.pid;
}

async function readChildPid(parent: ChildProcessWithoutNullStreams): Promise<number> {
  return new Promise((resolve, reject) => {
    parent.once("error", reject);
    parent.stdout.once("data", (chunk) => resolve(Number.parseInt(chunk.toString().trim(), 10)));
  });
}

async function readChildPids(parent: ChildProcessWithoutNullStreams): Promise<[number, number]> {
  return new Promise((resolve, reject) => {
    parent.once("error", reject);
    parent.stdout.once("data", (chunk) => {
      const pids = chunk.toString().trim().split(/\s+/).map((value: string) => Number.parseInt(value, 10));
      resolve([pids[0], pids[1]]);
    });
  });
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (caught) {
    if (caught instanceof Error && "code" in caught && caught.code === "ESRCH") {
      return false;
    }
    throw caught;
  }
}

function killProcessGroup(groupId: number): void {
  try {
    process.kill(-groupId, "SIGKILL");
  } catch (caught) {
    if (!(caught instanceof Error) || !("code" in caught) || caught.code !== "ESRCH") {
      throw caught;
    }
  }
}
