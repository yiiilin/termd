import { expect, test } from "@playwright/test";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { stopProcess } from "./real-relay-fixture";

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
