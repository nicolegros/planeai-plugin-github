import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

const manifest = JSON.parse(fs.readFileSync(new URL("../planeai-plugin.json", import.meta.url), "utf8"));
const hostCapabilities = new Set([
  "settings", "projects.read", "sessions.read", "sessions.repository-context", "sessions.prompt",
  "session-events", "sessions.actions", "sessions.advisories", "sessions.complete", "tasks.read",
  "tasks.create", "tasks.transition", "task-events",
]);
const backgroundFields = ["method", "interval_setting", "default_interval_ms"];

test("manifest uses PlaneAI PluginBackgroundService schema and documented capability names", () => {
  assert.deepEqual(Object.keys(manifest.background_service).sort(), backgroundFields.sort());
  assert.equal(manifest.background_service.method, "github.reconcile");
  assert.equal(manifest.background_service.interval_setting, "github_reconciliation_interval_ms");
  assert.ok(Number.isInteger(manifest.background_service.default_interval_ms));
  assert.ok(manifest.background_service.default_interval_ms > 0);
  assert.ok(manifest.capabilities.every((capability) => hostCapabilities.has(capability)));
  assert.ok(manifest.capabilities.includes("tasks.transition"));
  assert.ok(manifest.capabilities.includes("tasks.read"));
});

test("manifest exposes the visual-only cached PR-check indicator", () => {
  assert.deepEqual(
    manifest.ui_contributions.find((contribution) => contribution.id === "pr-check-indicator"),
    { id: "pr-check-indicator", label: "GitHub CI", placement: "session.indicator", entrypoint: "ui/indicator.js" },
  );
});

test("manifest exposes the GitHub transition preferences UI", () => {
  assert.deepEqual(
    manifest.ui_contributions.find((contribution) => contribution.id === "github-settings"),
    { id: "github-settings", label: "GitHub", placement: "preferences", entrypoint: "ui/settings.js" },
  );
});
