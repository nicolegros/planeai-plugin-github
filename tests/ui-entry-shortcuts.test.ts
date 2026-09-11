import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const entry = await readFile(new URL("../../ui/entry.ts", import.meta.url), "utf8");
const titlebar = await readFile(new URL("../../ui/titlebar.ts", import.meta.url), "utf8");

test("GitHub session panel uses the compact PR layout and capture-phase shortcuts", () => {
  assert.match(entry, /\.header \{ display:flex; align-items:center; gap:var\(--planeai-space-2\); padding:0 0/);
  assert.match(entry, /\.checks-heading \{ display:flex; align-items:center;/);
  assert.match(entry, /\.strategies \{ display:flex; gap:6px;/);
  assert.match(entry, /\.merge \{ width:100%; min-height:34px;/);
  assert.match(entry, /\.footer \{ display:flex; align-items:center; justify-content:space-between;/);
  assert.match(entry, /function cycleMergeStrategy\(\)/);
  assert.match(entry, /function confirmFocusedMergeStrategy\(\)/);
  assert.match(entry, /window\.addEventListener\("keydown", handleKeydown, true\)/);
  assert.match(entry, /window\.removeEventListener\("keydown", handleKeydown, true\)/);
  assert.doesNotMatch(entry, /page\.addEventListener\("keydown", handleKeydown\)/);
  assert.doesNotMatch(entry, /document\.addEventListener\("keydown", handleKeydown\)/);
  assert.match(entry, /editableTarget\(event\.target\)/);
  assert.match(entry, /\["r", "c", "o", "shift\+r", "s", "f"\]/);
  assert.match(entry, /choice\.dataset\.mergeStrategy = strategy/);
  assert.match(entry, /merge\.dataset\.mergeConfirm = ""/);
  assert.match(entry, /const reportContentHeight = \(\) => \{/);
  assert.match(entry, /window\.parent\.postMessage\(\{ type: "content-height", height \}, "\*"\)/);
  assert.match(entry, /const contentObserver = new MutationObserver/);
  assert.match(entry, /contentObserver\.disconnect\(\)/);
  assert.match(entry, /await call\("github\.merge", \{ strategy: selectedStrategy \}\)/);
  assert.match(entry, /await call\("github\.sendFailureLogs"\)/);
  assert.match(entry, /context\.host\.data\.notify\("CI failures sent to agent", "success"\)/);
  assert.doesNotMatch(entry, /call\("github\.failureLogs"\)/);
  assert.doesNotMatch(entry, /render\(result\.message/);
});

test("GitHub titlebar keeps the compact ready/create control", () => {
  assert.match(titlebar, /width:fit-content; max-width:100%; min-height:25px; height:25px;/);
  assert.match(titlebar, /button\[data-state="ready"\] \{ color:var\(--planeai-success\); background:rgba\(63,185,80,\.18\); \}/);
  assert.match(titlebar, /button\[data-state="create"\] \{ border-color:var\(--planeai-border\); padding:0 10px; \}/);
  assert.match(titlebar, /setButton\(number \? `PR #\$\{number\}` : "Pull request", "ready", false\)/);
  assert.match(titlebar, /context\.host\.navigation\.open\("github", "pull-request"\)/);
  assert.doesNotMatch(titlebar, /data-state="merged"|data-state="draft"|data-state="closed"/);
});
