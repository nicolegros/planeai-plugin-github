import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

const workflow = fs.readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8");

test("release publish commands receive explicit repository context", () => {
  const uploadStep = workflow.match(/- name: Upload release assets\r?\n(?<body>[\s\S]*?)(?=\r?\n      - name: Publish release)/)?.groups?.body;
  const publishStep = workflow.match(/- name: Publish release\r?\n(?<body>[\s\S]*)/)?.groups?.body;

  assert.match(uploadStep ?? "", /GH_REPO: \$\{\{ github\.repository \}\}/);
  assert.match(publishStep ?? "", /GH_REPO: \$\{\{ github\.repository \}\}/);
});

test("release matrix builds every manifest backend platform", () => {
  const manifest = JSON.parse(fs.readFileSync(new URL("../planeai-plugin.json", import.meta.url), "utf8"));
  const platforms = [...workflow.matchAll(/^\s+platform: (\S+)\r?$/gm)].map((match) => match[1]).sort();

  assert.deepEqual(platforms, ["linux-x64", "macos-arm64", "macos-x64", "windows-x64"]);
  assert.deepEqual(platforms, Object.keys(manifest.backend_entrypoints).sort());
});

test("release packages stage every bundled manifest UI entrypoint", () => {
  const manifest = JSON.parse(fs.readFileSync(new URL("../planeai-plugin.json", import.meta.url), "utf8"));
  const makefile = fs.readFileSync(new URL("../Makefile", import.meta.url), "utf8");
  const bundled = makefile.match(/^UI_ENTRYPOINTS := (.+)$/m)?.[1].split(/\s+/).sort();
  const entrypoints = manifest.ui_contributions.map((contribution) => contribution.entrypoint.match(/^ui\/(.+)\.js$/)?.[1]).sort();

  assert.deepEqual(bundled, entrypoints);
  for (const name of entrypoints) {
    assert.match(workflow, new RegExp(`cp [^\\n]*build/package/ui/${name}\\.js`), `${name}.js is staged on Unix`);
    assert.match(workflow, new RegExp(`Copy-Item build/package/ui/${name}\\.js "\\$root/ui/${name}\\.js"`), `${name}.js is staged on Windows`);
  }
});
