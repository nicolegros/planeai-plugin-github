import assert from "node:assert/strict";
import test from "node:test";
import {
  currentPullRequest,
  displayOrder,
  foreignRepository,
  moveSelection,
  resolveSelection,
  stackOrder,
  summarize,
  tooltipLines,
  worstCheck,
  type PullRequest,
} from "../ui/pull-requests.js";

const pr = (number: number, overrides: Partial<PullRequest> = {}): PullRequest => ({
  host: "github.com",
  repository: "o/r",
  number,
  url: `https://github.com/o/r/pull/${number}`,
  source: "manual",
  state: "open",
  title: `Change ${number}`,
  checks: null,
  head_branch: `head-${number}`,
  base_branch: "main",
  updated_at: `2026-10-${String(number).padStart(2, "0")}T00:00:00Z`,
  ...overrides,
});

const stack = [
  pr(3, { head_branch: "c", base_branch: "b", state: "draft" }),
  pr(1, { head_branch: "a", base_branch: "main" }),
  pr(2, { head_branch: "b", base_branch: "a" }),
];

test("the current pull request is the single open one", () => {
  assert.equal(currentPullRequest([pr(9, { state: "merged" }), pr(4), pr(8, { state: "closed" })])?.number, 4);
  assert.equal(currentPullRequest([pr(4, { state: "draft" }), pr(9, { state: "merged" })])?.number, 4, "a draft is still open");
  assert.equal(currentPullRequest([]), null);
});

test("with several open pull requests the top of their stack is current", () => {
  assert.equal(currentPullRequest(stack)?.number, 3);
  assert.deepEqual(stackOrder(stack)?.map((item) => item.number), [1, 2, 3]);
});

test("otherwise the most recently updated pull request is current, preferring open ones", () => {
  assert.equal(currentPullRequest([pr(2), pr(7), pr(5)])?.number, 7);
  assert.equal(currentPullRequest([pr(2), pr(5), pr(9, { state: "merged" })])?.number, 5);
  assert.equal(currentPullRequest([pr(2, { state: "merged" }), pr(5, { state: "closed" })])?.number, 5);
});

test("a stack is one chain in one repository", () => {
  assert.equal(stackOrder([pr(1)]), null);
  assert.equal(stackOrder([pr(1, { head_branch: "a" }), pr(2, { base_branch: "a" }), pr(3, { base_branch: "a" })]), null, "a fork is not a chain");
  assert.equal(stackOrder([pr(1, { head_branch: "a" }), pr(2, { base_branch: "a", repository: "x/y" })]), null);
  assert.equal(stackOrder([pr(1, { head_branch: "a" }), pr(2, { base_branch: "a" }), pr(5)]), null, "an unrelated PR breaks the stack");
});

test("the worst check across open pull requests wins", () => {
  assert.equal(worstCheck([pr(1, { checks: "passing" }), pr(2, { checks: "pending" })]), "pending");
  assert.equal(worstCheck([pr(1, { checks: "pending" }), pr(2, { checks: "failing" }), pr(3, { checks: "passing" })]), "failing");
  assert.equal(worstCheck([pr(1, { checks: "passing" }), pr(2, { checks: "failing", state: "merged" })]), "passing", "closed PRs do not count");
  assert.equal(worstCheck([pr(1)]), null);
});

test("the summary counts the other pull requests and says when they stack", () => {
  assert.deepEqual(summarize([pr(4, { checks: "passing" })]), { current: pr(4, { checks: "passing" }), others: 0, stacked: false, checks: "passing" });
  const stacked = summarize(stack);
  assert.equal(stacked?.current.number, 3);
  assert.equal(stacked?.others, 2);
  assert.equal(stacked?.stacked, true);
  assert.equal(summarize([pr(4), pr(6)])?.stacked, false);
  assert.equal(summarize([]), null);
});

test("tooltip lines list every pull request, toned by CI then state", () => {
  assert.deepEqual(tooltipLines([pr(1, { checks: "failing" }), pr(2, { state: "merged", checks: "failing" }), pr(3, { checks: "passing", title: "" })]), [
    { text: "#3 · CI passing", tone: "success" },
    { text: "#1 · CI failing · Change 1", tone: "danger" },
    { text: "#2 · Merged · Change 2", tone: "info" },
  ]);
  assert.deepEqual(tooltipLines(stack).map((line) => line.text.split(" ")[0]), ["#3", "#2", "#1"], "a stack reads from its top down");
  assert.equal(tooltipLines([pr(4, { state: "draft", checks: "pending" })])[0].text, "#4 · Draft, CI pending · Change 4");
  assert.equal(tooltipLines([pr(4, { state: "open", checks: null })])[0].text, "#4 · Open · Change 4");
});

test("display order puts open pull requests first and stacks top down", () => {
  assert.deepEqual(displayOrder([pr(5, { state: "merged" }), pr(2), pr(4)]).map((item) => item.number), [4, 2, 5]);
  assert.deepEqual(displayOrder(stack).map((item) => item.number), [3, 2, 1]);
});

test("the repository is named only when it differs from the session's", () => {
  assert.equal(foreignRepository(pr(1), "O/R"), null);
  assert.equal(foreignRepository(pr(1, { repository: "x/y" }), "o/r"), "x/y");
  assert.equal(foreignRepository(pr(1, { repository: "o/other" }), "o/r"), "other", "a shared owner is left out");
  assert.equal(foreignRepository(pr(1), null), null);
});

test("the panel highlights the session's own pull request first, else the current one", () => {
  const prs = [pr(1), pr(2), pr(3)];
  assert.equal(resolveSelection(prs, null, prs[0].url)?.number, 1, "the session's own pull request wins");
  assert.equal(resolveSelection(prs, null, null)?.number, currentPullRequest(prs)?.number);
  assert.equal(resolveSelection(prs, prs[2].url, prs[0].url)?.number, 3, "a highlight survives a refresh");
  assert.equal(resolveSelection(prs, "https://github.com/o/r/pull/99", prs[0].url)?.number, 1, "a vanished highlight falls back");
  assert.equal(resolveSelection([], null, null), null);
});

test("moving the highlight follows display order and stops at the ends", () => {
  const prs = [pr(1), pr(2, { state: "merged" }), pr(3)];
  const order = displayOrder(prs).map((item) => item.number);
  const from = (number: number, delta: 1 | -1) => moveSelection(prs, `https://github.com/o/r/pull/${number}`, delta)?.number;
  assert.equal(from(order[0], 1), order[1]);
  assert.equal(from(order[1], -1), order[0]);
  assert.equal(from(order[0], -1), order[0], "the first row stays put going up");
  assert.equal(from(order[2], 1), order[2], "the last row stays put going down");
  assert.equal(moveSelection([], null, 1), null);
});
