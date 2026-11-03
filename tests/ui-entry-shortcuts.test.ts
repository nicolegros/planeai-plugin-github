import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const entry = await readFile(new URL("../../ui/entry.ts", import.meta.url), "utf8");
const settings = await readFile(new URL("../../ui/settings.ts", import.meta.url), "utf8");
const titlebar = await readFile(new URL("../../ui/titlebar.ts", import.meta.url), "utf8");
const meta = await readFile(new URL("../../ui/meta.ts", import.meta.url), "utf8");
const pill = await readFile(new URL("../../ui/pill.ts", import.meta.url), "utf8");

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
  assert.match(entry, /\["r", "c", "l", "o", "u", "shift\+r", "s", "f"\]/);
  assert.match(entry, /choice\.dataset\.mergeStrategy = strategy/);
  assert.match(entry, /merge\.dataset\.mergeConfirm = ""/);
  assert.match(entry, /const reportContentHeight = \(\) => \{/);
  assert.match(entry, /window\.parent\.postMessage\(\{ type: "content-height", height \}, "\*"\)/);
  assert.match(entry, /const contentObserver = new MutationObserver/);
  assert.match(entry, /contentObserver\.disconnect\(\)/);
  assert.match(entry, /call\("github\.merge", \{ strategy: selectedStrategy, url: pr\.url \}\)/);
  assert.match(entry, /Link pull request/);
  assert.doesNotMatch(entry, /github\.recipients/);
  assert.doesNotMatch(entry, /Recipient session/);
  assert.doesNotMatch(entry, /selectedRecipientId/);
  assert.match(entry, /recipient_session_id: recipient\.id/);
  assert.doesNotMatch(entry, /call\("github\.failureLogs"\)/);
  assert.doesNotMatch(entry, /render\(result\.message/);
});

test("GitHub session meta pill reads only the cached summary and reports its size and tooltip", () => {
  assert.match(meta, /context\.host\.call\("github\.summary"/);
  assert.doesNotMatch(meta, /github\.status/);
  assert.doesNotMatch(meta, /navigation\./);
  assert.match(meta, /type: "content-width", width: 0/);
  assert.match(meta, /type: "content-width", width: Math\.ceil\(pill\.getBoundingClientRect\(\)\.width\)/);
  assert.match(meta, /type: "tooltip", lines: tooltipLines\(prs\)/);
  assert.match(meta, /type: "tooltip", lines: \[\]/);
  assert.match(meta, /context\.host\.data\.onChanged/);
  assert.match(pill, /width:max-content/);
  assert.doesNotMatch(pill, /#[0-9a-f]{3,6}\b|rgba?\(/i, "the pill colors come from theme tokens only");
  assert.match(pill, /var\(--planeai-info\)/);
});

test("GitHub titlebar shows the pill summary or the create control", () => {
  assert.match(titlebar, /context\.host\.call\("github\.summary", \{ session_id: sessionId, check_repository: true \}\)/, "the titlebar reads the cached summary");
  assert.doesNotMatch(titlebar, /github\.status/, "the titlebar never fetches pull request details");
  assert.match(titlebar, /width:fit-content; max-width:100%; min-height:25px; height:25px;/);
  assert.match(titlebar, /const pill = renderPill\(prs\)/, "the titlebar shares the sidebar pill");
  assert.match(titlebar, /setButton\("＋ Create PR", "create", false/);
  assert.match(titlebar, /button\[data-state="create"\] \{ border-color:var\(--planeai-border\); padding:0 10px; \}/);
  assert.match(titlebar, /context\.host\.navigation\.open\("github", "pull-request"\)/);
  assert.doesNotMatch(titlebar, /#bc8cff|rgba\(/, "titlebar colors come from theme tokens only");
});

test("GitHub UI links existing PRs and configures automatic task transitions", () => {
  assert.match(settings, /context\.host\.call\("github\.settings"\)/);
  assert.match(settings, /context\.host\.call\("github\.settings\.update"/);
  assert.match(settings, /When a pull request opens/);
  assert.match(settings, /When a pull request merges/);
});

test("GitHub session panel lists every pull request with per-row actions", () => {
  assert.match(entry, /for \(const pr of displayOrder\(prs\)\) list\.append\(pullRequestRow\(pr, current, selected\)\)/);
  assert.match(entry, /if \(isSelected\) remove\.dataset\.shortcut = "u"/, "U unlinks or dismisses the highlighted row");
  assert.match(entry, /const detail = detailOf\(selected\);/, "the details follow the highlight");
  assert.match(entry, /call\("github\.unlink", \{ url: pr\.url \}\)/);
  assert.match(entry, /foreignRepository\(pr, status\(\)\?\.repository\)/, "rows name a repository only when it differs");
  assert.match(entry, /\.pr-repo \{ flex:0 1000 auto; min-width:4ch; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;/, "the repository name shrinks before the title does");
  assert.match(entry, /repo\.title = repository/, "a shortened repository name stays readable on hover");
  assert.match(entry, /SOURCE_LABELS\[pr\.source\]/);
  assert.match(entry, /button\("↗", \(\) => context\.host\.navigation\.openExternal\(pr\.url\)\)/);
  assert.match(entry, /grid-template-columns:8px 42px minmax\(0, 1fr\) 12px 52px 92px/, "fixed columns keep numbers, titles, and actions aligned across rows");
});
