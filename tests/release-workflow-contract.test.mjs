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
