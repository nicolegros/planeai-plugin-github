import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const entry = await readFile(new URL("../ui/entry.js", import.meta.url), "utf8");

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
  assert.match(entry, /await call\("github\.merge", \{ strategy: selectedStrategy \}\)/);
});
