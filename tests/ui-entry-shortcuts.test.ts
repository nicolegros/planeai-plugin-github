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
  assert.match(entry, /function renderLink\(\)/);
  assert.match(entry, /Link pull request/);
  assert.match(entry, /await call\("github\.link", \{ url: urlField\.value \}\)/);
  assert.match(entry, /const recipient = await context\.host\.recipient\.getFocusedAgentSession\(\)/);
  assert.doesNotMatch(entry, /github\.recipients/);
  assert.doesNotMatch(entry, /Recipient session/);
  assert.doesNotMatch(entry, /selectedRecipientId/);
  assert.match(entry, /recipient_session_id: recipient\.id/);
  assert.match(entry, /await call\("github\.sendFailureLogs", \{ recipient_session_id: recipient\.id, url: pr\.url \}\)/);
  assert.match(entry, /context\.host\.data\.notify\("CI failures sent to agent", "success"\)/);
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
  assert.match(titlebar, /context\.host\.data\?\.onChanged\?\./, "the titlebar follows data changes");
  assert.match(titlebar, /width:fit-content; max-width:100%; min-height:25px; height:25px;/);
  assert.match(titlebar, /const pill = renderPill\(prs\)/, "the titlebar shares the sidebar pill");
  assert.match(titlebar, /setButton\("＋ Create PR", "create", false/);
  assert.match(titlebar, /button\[data-state="create"\] \{ border-color:var\(--planeai-border\); padding:0 10px; \}/);
  assert.match(titlebar, /context\.host\.navigation\.open\("github", "pull-request"\)/);
  assert.doesNotMatch(titlebar, /#bc8cff|rgba\(/, "titlebar colors come from theme tokens only");
});

test("GitHub UI links existing PRs and configures automatic task transitions", () => {
  assert.match(entry, /function renderLink\(\)/);
  assert.match(entry, /call\("github\.link", \{ url: urlField\.value \}\)/);
  assert.match(entry, /button\("Link pull request", \(\) => \{ linking = true; render\(\); \}, \{ shortcut: "l" \}\)/, "L opens the link flow");
  assert.match(entry, /appendFooter\(\[\["L", "link"\], \["Esc", "close"\]\]\)/, "the link form documents its keyboard flow");
  assert.match(entry, /if \(!branchHasPullRequest\(pullRequests\(\), status\(\)\?\.branch\)\) \{\s*actions\.append\(button\("Create PR"[\s\S]*?\{ shortcut: "c" \}\)\);\s*\}\s*actions\.append\(button\("Link pull request"/, "Create PR shows only while the branch has no pull request, beside Link");
  assert.match(entry, /function installFormKeyboard\(form, focusField, cancel\)/, "forms share keyboard behavior");
  assert.match(entry, /requestAnimationFrame\(\(\) => focusField\.focus\(\)\)/, "both forms autofocus their supplied first field");
  assert.match(entry, /event\.key === "Escape"/, "Escape cancels a form");
  assert.match(entry, /\(event\.metaKey \|\| event\.ctrlKey\) && event\.key === "Enter"/, "platform-modifier Enter submits from multi-line fields");
  assert.match(entry, /installFormKeyboard\(form, titleField, \(\) => \{ creating = false; render\(\); \}\)/, "Create PR wires shared form keyboard behavior");
  assert.match(entry, /installFormKeyboard\(form, urlField, \(\) => \{ linking = false; render\(\); \}\)/, "Link PR wires shared form keyboard behavior");
  assert.match(entry, /const mergeMethods = pr\.merge_methods\.filter/);
  assert.match(entry, /for \(const strategy of mergeMethods\)/);
  assert.doesNotMatch(entry, /for \(const strategy of \["squash", "merge", "rebase"\]\)/);
  assert.match(settings, /context\.host\.call\("github\.settings"\)/);
  assert.match(settings, /context\.host\.call\("github\.settings\.update"/);
  assert.match(settings, /When a pull request opens/);
  assert.match(settings, /When a pull request merges/);
});

test("GitHub session panel lists every pull request with per-row actions", () => {
  assert.match(entry, /for \(const pr of displayOrder\(prs\)\) list\.append\(pullRequestRow\(pr, current, selected\)\)/);
  assert.match(entry, /list\.setAttribute\("role", "listbox"\)/, "the rows are one listbox");
  assert.match(entry, /row\.setAttribute\("aria-selected", String\(isSelected\)\)/);
  assert.match(entry, /const SELECTION_KEYS: Record<string, 1 \| -1> = \{ ArrowDown: 1, j: 1, ArrowUp: -1, k: -1 \}/, "arrows and j/k move the highlight");
  assert.match(entry, /if \(isSelected\) remove\.dataset\.shortcut = "u"/, "U unlinks or dismisses the highlighted row");
  assert.match(entry, /const detail = detailOf\(selected\);/, "the details follow the highlight");
  assert.match(entry, /const label = pr\.source === "stack" \? "Dismiss" : "Unlink";/, "stack rows dismiss, other rows unlink");
  assert.match(entry, /if \(pr\.source !== "branch"\)/, "branch rows cannot be unlinked");
  assert.match(entry, /call\("github\.unlink", \{ url: pr\.url \}\)/);
  assert.match(entry, /foreignRepository\(pr, status\(\)\?\.repository\)/, "rows name a repository only when it differs");
  assert.match(entry, /\.pr-repo \{ flex:0 1000 auto; min-width:4ch; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;/, "the repository name shrinks before the title does");
  assert.match(entry, /repo\.title = repository/, "a shortened repository name stays readable on hover");
  assert.match(entry, /SOURCE_LABELS\[pr\.source\]/);
  assert.match(entry, /button\("↗", \(\) => context\.host\.navigation\.openExternal\(pr\.url\)\)/);
  assert.match(entry, /grid-template-columns:8px 42px minmax\(0, 1fr\) 12px 52px 92px/, "fixed columns keep numbers, titles, and actions aligned across rows");
  assert.match(entry, /if \(next\.applicable && next\.changed\) void dataChanged\(\)/, "a refresh that changed stored PRs refreshes the sidebar pills");
  assert.match(entry, /appendFooter\(panelHints\(\{ prs, selected,/, "the footer hints come from panelHints");
  assert.match(entry, /if \(disposed \|\| current !== generation\) return;/, "a stale github.status answer never renders");
  assert.match(entry, /if \(context\.host\.data\.changed\) await dataChanged\(\);\s*else await load\(\);/, "a mutation refreshes once: the host remounts the panel on data changes");
});
