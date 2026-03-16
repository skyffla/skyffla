import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { probeBinaryVersion } from "../src/index.js";

test("probeBinaryVersion parses version text from stdout for a short-lived process", async () => {
  const fixture = path.join(
    path.dirname(fileURLToPath(import.meta.url)),
    "fixtures",
    "fake-skyffla-version-stdout",
  );

  const version = await probeBinaryVersion(fixture);
  assert.equal(version, "1.2.3");
});
