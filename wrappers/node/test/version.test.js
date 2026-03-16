import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { probeBinaryVersion } from "../src/index.js";

test("probeBinaryVersion parses version text from stderr when stdout is empty", async () => {
  const fixture = path.join(
    path.dirname(fileURLToPath(import.meta.url)),
    "fixtures",
    "fake-skyffla-version-stderr",
  );

  const version = await probeBinaryVersion(fixture);
  assert.equal(version, "1.2.3");
});
