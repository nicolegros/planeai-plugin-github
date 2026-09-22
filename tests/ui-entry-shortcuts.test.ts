import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const entry = await readFile(new URL("../../ui/entry.ts", import.meta.url), "utf8");
const settings = await readFile(new URL("../../ui/settings.ts", import.meta.url), "utf8");
const titlebar = await readFile(new URL("../../ui/titlebar.ts", import.meta.url), "utf8");
const indicator = await readFile(new URL("../../ui/indicator.ts", import.meta.url), "utf8");

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
  assert.match(entry, /\["r", "c", "l", "o", "shift\+r", "s", "f"\]/);
  assert.match(entry, /choice\.dataset\.mergeStrategy = strategy/);
  assert.match(entry, /merge\.dataset\.mergeConfirm = ""/);
  assert.match(entry, /const reportContentHeight = \(\) => \{/);
  assert.match(entry, /window\.parent\.postMessage\(\{ type: "content-height", height \}, "\*"\)/);
  assert.match(entry, /const contentObserver = new MutationObserver/);
  assert.match(entry, /contentObserver\.disconnect\(\)/);
  assert.match(entry, /await call\("github\.merge", \{ strategy: selectedStrategy \}\)/);
  assert.match(entry, /const recipient = await context\.host\.recipient\.getFocusedAgentSession\(\)/);
  assert.doesNotMatch(entry, /github\.recipients/);
  assert.doesNotMatch(entry, /Recipient session/);
  assert.doesNotMatch(entry, /selectedRecipientId/);
  assert.match(entry, /recipient_session_id: recipient\.id/);
  assert.match(entry, /await call\("github\.sendFailureLogs", \{ recipient_session_id: recipient\.id \}\)/);
  assert.match(entry, /context\.host\.data\.notify\("CI failures sent to agent", "success"\)/);
  assert.doesNotMatch(entry, /call\("github\.failureLogs"\)/);
  assert.doesNotMatch(entry, /render\(result\.message/);
});

test("GitHub check indicator reads only cached summaries and remains visual-only", () => {
  assert.match(indicator, /context\.host\.call\("github\.indicator"/);
  assert.doesNotMatch(indicator, /github\.status/);
  assert.doesNotMatch(indicator, /navigation\./);
  assert.match(indicator, /role", "img"/);
  assert.match(indicator, /GitHub CI passing/);
  assert.match(indicator, /<circle cx="12" cy="12" r="10"\/>/);
  assert.match(indicator, /m9 12 2 2 4-4/);
  assert.match(indicator, /m15 9-6 6/);
  assert.match(indicator, /github-check-dot/);
  assert.match(indicator, /background:#f59e0b/);
  assert.match(indicator, /animation:pulse-dot 1\.6s ease-in-out infinite/);
  assert.match(indicator, /opacity:\.4;transform:scale\(\.78\)/);
  assert.match(indicator, /type: "content-width", width/);
  assert.match(indicator, /reportContentWidth\(0\)/);
  assert.match(indicator, /reportContentWidth\(16\)/);
  assert.match(indicator, /context\.host\.data\.onChanged/);
  assert.match(indicator, /prefers-reduced-motion:reduce/);
});

test("GitHub titlebar keeps the compact ready/create control", () => {
  assert.match(titlebar, /width:fit-content; max-width:100%; min-height:25px; height:25px;/);
  assert.match(titlebar, /button\[data-state="ready"\] \{ color:var\(--planeai-success\); background:rgba\(63,185,80,\.18\); \}/);
  assert.match(titlebar, /button\[data-state="merged"\] \{ color:#bc8cff; background:rgba\(188,140,255,\.18\); \}/, "merged PRs retain the established violet titlebar treatment");
  assert.match(titlebar, /const state = status\.pr\.state === "merged" \? "merged" : "ready";/, "merged status selects the violet state");
  assert.match(titlebar, /button\[data-state="create"\] \{ border-color:var\(--planeai-border\); padding:0 10px; \}/);
  assert.match(titlebar, /setButton\(number \? `PR #\$\{number\}` : "Pull request", state, false\)/);
  assert.match(titlebar, /context\.host\.navigation\.open\("github", "pull-request"\)/);
});

test("GitHub UI links existing PRs and configures automatic task transitions", () => {
  assert.match(entry, /function renderLink\(\)/);
  assert.match(entry, /call\("github\.link", \{ url: urlField\.value \}\)/);
  assert.match(entry, /Link existing pull request/, "the no-PR panel exposes linking");
  assert.match(entry, /button\("Link existing pull request", \(\) => \{ linking = true; render\(\); \}, \{ shortcut: "l" \}\)/, "L opens the link flow");
  assert.match(entry, /appendFooter\(\[\["L", "link"\], \["Esc", "close"\]\]\)/, "the link form documents its keyboard flow");
  assert.match(entry, /const actions = document\.createElement\("div"\); actions\.className = "form-actions";\s*actions\.append\(\s*button\("Create pull request"[\s\S]*?button\("Link existing pull request"/, "Create and Link use the spaced action group");
  assert.match(entry, /\["r", "c", "l", "o", "shift\+r", "s", "f"\]/, "the global panel shortcut handler recognizes L");
  assert.match(entry, /function installFormKeyboard\(form, focusField, cancel\)/, "forms share keyboard behavior");
  assert.match(entry, /requestAnimationFrame\(\(\) => focusField\.focus\(\)\)/, "both forms autofocus their supplied first field");
  assert.match(entry, /event\.key === "Escape"/, "Escape cancels a form");
  assert.match(entry, /\(event\.metaKey \|\| event\.ctrlKey\) && event\.key === "Enter"/, "platform-modifier Enter submits from multi-line fields");
  assert.match(entry, /installFormKeyboard\(form, titleField, \(\) => \{ creating = false; render\(\); \}\)/, "Create PR wires shared form keyboard behavior");
  assert.match(entry, /installFormKeyboard\(form, urlField, \(\) => \{ linking = false; render\(\); \}\)/, "Link PR wires shared form keyboard behavior");
  assert.match(entry, /const mergeMethods = Array\.isArray\(pr\.merge_methods\)/);
  assert.match(entry, /for \(const strategy of mergeMethods\)/);
  assert.doesNotMatch(entry, /for \(const strategy of \["squash", "merge", "rebase"\]\)/);
  assert.match(settings, /context\.host\.call\("github\.settings"\)/);
  assert.match(settings, /context\.host\.call\("github\.settings\.update"/);
  assert.match(settings, /When a pull request opens/);
  assert.match(settings, /When a pull request merges/);
});
