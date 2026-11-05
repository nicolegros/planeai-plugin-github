import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

const manifest = JSON.parse(fs.readFileSync(new URL("../planeai-plugin.json", import.meta.url), "utf8"));
const hostCapabilities = new Set([
  "settings", "projects.read", "sessions.read", "sessions.repository-context", "sessions.prompt",
  "session-events", "sessions.actions", "sessions.advisories", "sessions.complete", "tasks.read",
  "tasks.create", "tasks.transition", "task-events", "tools",
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

test("manifest declares the session.meta pull-request pill", () => {
  assert.deepEqual(
    manifest.ui_contributions.find((contribution) => contribution.placement === "session.meta"),
    { id: "pull-request-meta", label: "Pull requests", placement: "session.meta", entrypoint: "ui/meta.js" },
  );
  assert.equal(manifest.ui_contributions.filter((contribution) => contribution.placement === "session.meta").length, 1);
  assert.ok(!manifest.ui_contributions.some((contribution) => contribution.placement === "session.indicator"), "the pill replaces the CI indicator");
  const panels = manifest.ui_contributions.filter((contribution) => contribution.placement === "session.panel");
  assert.equal(panels[0]?.id, "pull-request", "clicking the pill opens the first session panel");
  assert.equal(panels[0]?.label, "Pull requests", "the panel titles every pull request of the task");
  assert.ok(manifest.capabilities.includes("sessions.read"), "task siblings come from the session list");
});

test("manifest UI contributions have unique ids and bundled entrypoints", () => {
  const ids = manifest.ui_contributions.map((contribution) => contribution.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const contribution of manifest.ui_contributions) {
    assert.ok(fs.existsSync(new URL(`../${contribution.entrypoint.replace(/\.js$/, ".ts")}`, import.meta.url)), contribution.entrypoint);
  }
});

test("manifest exposes the GitHub transition preferences UI", () => {
  assert.deepEqual(
    manifest.ui_contributions.find((contribution) => contribution.id === "github-settings"),
    { id: "github-settings", label: "GitHub", placement: "preferences", entrypoint: "ui/settings.js" },
  );
});

test("manifest declares host API v6 and the link_pull_request tool as a non-destructive write", () => {
  assert.equal(manifest.host_api_version, "planeai.plugin-host.v6");
  assert.ok(manifest.capabilities.includes("tools"));
  assert.deepEqual(manifest.tools.map((tool) => tool.name), ["link_pull_request"]);
  const [tool] = manifest.tools;
  assert.deepEqual(Object.keys(tool).sort(), ["annotations", "description", "inputSchema", "name"]);
  assert.deepEqual(tool.inputSchema.required, ["url"]);
  assert.equal(tool.annotations.readOnlyHint, false);
  assert.equal(tool.annotations.destructiveHint, false, "PlaneAI pre-approves only tools that say they are not destructive");
  assert.equal(tool.annotations.idempotentHint, true);
});
