import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const injector = path.join(repositoryRoot, "scripts", "inject-release-version.mjs");

const read = (file) => fs.readFileSync(path.join(repositoryRoot, file), "utf8");

test("injects the release version when Cargo.lock uses CRLF line endings", (t) => {
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), "planeai-plugin-release-version-"));
  t.after(() => fs.rmSync(fixture, { force: true, recursive: true }));

  for (const file of ["package.json", "planeai-plugin.json", "Cargo.toml"]) {
    fs.writeFileSync(path.join(fixture, file), read(file));
  }
  fs.writeFileSync(path.join(fixture, "Cargo.lock"), read("Cargo.lock").replace(/\n/g, "\r\n"));

  execFileSync(process.execPath, [injector, "v1.2.3"], { cwd: fixture });

  assert.equal(JSON.parse(fs.readFileSync(path.join(fixture, "package.json"), "utf8")).version, "1.2.3");
  assert.equal(JSON.parse(fs.readFileSync(path.join(fixture, "planeai-plugin.json"), "utf8")).version, "1.2.3");
  assert.match(fs.readFileSync(path.join(fixture, "Cargo.toml"), "utf8"), /version = "1\.2\.3"/);
  assert.match(
    fs.readFileSync(path.join(fixture, "Cargo.lock"), "utf8"),
    /name = "planeai-plugin-github"\r\nversion = "1\.2\.3"/,
  );
});
