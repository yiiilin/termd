// @vitest-environment node

import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { build } from "vite";

const frontendRoot = resolve(import.meta.dirname, "../..");
const sourceFont = join(
  frontendRoot,
  "src/assets/fonts/HarmonyOS_Sans_SC_Regular.ttf",
);
const publicLicense = join(
  frontendRoot,
  "public/fonts/HarmonyOS_Sans_SC_LICENSE.txt",
);
let outputDir: string | undefined;

afterAll(async () => {
  if (outputDir) {
    await rm(outputDir, { recursive: true, force: true });
  }
});

describe("HarmonyOS font distribution", () => {
  it(
    "emits a hashed byte-identical font and preserves the public license",
    async () => {
      outputDir = await mkdtemp(join(tmpdir(), "termd-font-build-"));
      await build({
        root: frontendRoot,
        logLevel: "silent",
        build: {
          outDir: outputDir,
          emptyOutDir: true,
        },
      });

      const assets = await readdir(join(outputDir, "assets"));
      const builtFont = assets.find((name) =>
        /^HarmonyOS_Sans_SC_Regular-[A-Za-z0-9_-]{8}\.ttf$/.test(name),
      );
      expect(builtFont).toBeDefined();
      await expect(
        readFile(join(outputDir, "assets", builtFont!)),
      ).resolves.toEqual(await readFile(sourceFont));
      await expect(
        readFile(join(outputDir, "fonts/HarmonyOS_Sans_SC_LICENSE.txt")),
      ).resolves.toEqual(await readFile(publicLicense));
    },
    120_000,
  );
});
