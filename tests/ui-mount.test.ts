import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { deferred, installDom, press, settle, type Deferred } from "./ui-harness.js";
import githubEntrypoint from "../ui/entry.js";
import githubTitlebarEntrypoint from "../ui/titlebar.js";
import type { PullRequest, PullRequestSource } from "../ui/pull-requests.js";

const url = (number: number) => `https://github.com/o/r/pull/${number}`;
const pr = (number: number, source: PullRequestSource, title = `Change ${number}`): PullRequest => ({
  host: "github.com", repository: "o/r", number, url: url(number), source, state: "open", title,
  checks: null, head_branch: `b${number}`, base_branch: "main", updated_at: `2026-10-0${9 - number}T00:00:00Z`,
});
const detail = (number: number) => ({ url: url(number), number, state: "open", checks: [], conflicting: false, merge_blocked: false, merge_methods: ["squash", "merge"] });
const status = (title = "Change") => ({
  applicable: true, remote: "https://github.com/o/r.git", repository: "o/r", branch: "b1",
  pull_requests: [pr(1, "branch", `${title} 1`), pr(2, "manual", `${title} 2`)],
  own_url: url(1), details: [detail(1), detail(2)], changed: false,
});

type HostCall = { method: string; params: Record<string, unknown>; reply: Deferred<unknown>; answered: boolean };

/** Mounts the panel with a host whose every call waits until the test answers it. */
function mountPanel(sessionId = "s1") {
  const calls: HostCall[] = [];
  let changed = 0;
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = githubEntrypoint.mount(root, {
    session: { id: sessionId, name: "Session" },
    host: {
      call: (method, params) => {
        const reply = deferred<unknown>();
        calls.push({ method, params, reply, answered: false });
        return reply.promise;
      },
      navigation: { openExternal: () => {} },
      recipient: { getFocusedAgentSession: async () => null },
      data: { changed: async () => { changed += 1; }, notify: () => {} },
    },
  });
  const pending = (method: string) => calls.filter((call) => call.method === method);
  return {
    root, dispose, calls, pending,
    changes: () => changed,
    async answer(method: string, value: unknown) {
      const call = pending(method).find((candidate) => !candidate.answered);
      call.answered = true;
      call.reply.resolve(value);
      await settle();
    },
  };
}

const list = (root: Element) => root.querySelector<HTMLElement>(".pr-list");
const selectedNumber = (root: Element) => root.querySelector('.pr-row[aria-selected="true"] .pr-number')?.textContent;

beforeEach(() => { installDom(); });

test("only the latest status refresh renders, whatever order the answers come in", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status("First"));
  panel.root.querySelector<HTMLButtonElement>("button.refresh").click();
  panel.root.querySelector<HTMLButtonElement>("button.refresh").click();
  const [, second, third] = panel.pending("github.status");
  third.reply.resolve(status("Third"));
  await settle();
  second.reply.resolve(status("Second"));
  await settle();
  assert.match(panel.root.querySelector(".pr-title").textContent, /^Third/);
});

test("a change refreshes through the host once, without another status call", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  assert.equal(panel.changes(), 1);
  assert.equal(panel.pending("github.status").length, 1, "the host remount loads the panel again");
});

test("the list is a grid whose focus survives re-renders and refreshes", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  const grid = list(panel.root);
  assert.equal(grid.getAttribute("role"), "grid");
  assert.ok([...grid.querySelectorAll(".pr-row")].every((row) => row.getAttribute("role") === "row"));
  assert.ok([...grid.querySelectorAll(".pr-row > *")].every((cell) => cell.getAttribute("role") === "gridcell"));
  assert.ok([...grid.querySelectorAll("button")].every((button) => button.tabIndex === -1), "row buttons stay out of the tab order");

  grid.focus();
  press("j");
  assert.equal(selectedNumber(panel.root), "#2");
  assert.equal(document.activeElement, list(panel.root), "focus moves to the re-rendered list");
  const active = document.getElementById(list(panel.root).getAttribute("aria-activedescendant"));
  assert.equal(active?.getAttribute("role"), "gridcell");
  assert.equal(active?.closest(".pr-row")?.getAttribute("aria-selected"), "true");

  press("r");
  assert.equal(document.activeElement, list(panel.root), "the list stays while refreshing");
  await panel.answer("github.status", status());
  assert.equal(document.activeElement, list(panel.root));
  assert.equal(selectedNumber(panel.root), "#2");
});

test("arrows on another control leave the highlight alone", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  const strategy = panel.root.querySelector<HTMLButtonElement>("button[data-merge-strategy]");
  strategy.focus();
  press("ArrowDown");
  assert.equal(selectedNumber(panel.root), "#1");
  strategy.blur();
  press("ArrowDown", document.body);
  assert.equal(selectedNumber(panel.root), "#2", "with nothing focused, arrows move the highlight");
});

test("the highlight survives the host remounting the panel", async () => {
  const first = mountPanel();
  await first.answer("github.status", status());
  list(first.root).focus();
  press("j");
  first.dispose();
  const second = mountPanel();
  await second.answer("github.status", status());
  assert.equal(selectedNumber(second.root), "#2");
  const other = mountPanel("s2");
  await other.answer("github.status", status());
  assert.equal(selectedNumber(other.root), "#1", "each session keeps its own highlight");
});

test("the titlebar reloads its summary when the plugin's data changes", async () => {
  const listeners: (() => void)[] = [];
  const answers = [{ applicable: true, pull_requests: [] }, { applicable: true, pull_requests: [pr(1, "branch")] }];
  const root = document.createElement("div");
  document.body.append(root);
  githubTitlebarEntrypoint.mount(root, {
    session: { id: "s1" },
    host: {
      call: async () => answers.shift(),
      navigation: { open: () => {} },
      data: { onChanged: (listener) => { listeners.push(listener); return () => {}; } },
    },
  });
  await settle();
  assert.equal(root.querySelector("button").dataset.state, "create");
  listeners.forEach((listener) => listener());
  await settle();
  assert.equal(root.querySelector("button").dataset.state, "pull-requests");
});

test("an open link form, its value, and the merge strategy survive a remount until the form closes", async () => {
  const first = mountPanel();
  await first.answer("github.status", status());
  first.root.querySelector<HTMLButtonElement>('button[data-merge-strategy="merge"]').click();
  press("l", document.body);
  const field = first.root.querySelector<HTMLInputElement>('input[type="url"]');
  field.value = "https://github.com/o/r/pull/9";
  field.dispatchEvent(new window.Event("input", { bubbles: true }));
  first.dispose();

  const second = mountPanel();
  await second.answer("github.status", status());
  assert.equal(second.root.querySelector<HTMLInputElement>('input[type="url"]')?.value, "https://github.com/o/r/pull/9");
  press("Escape", second.root.querySelector('input[type="url"]'));
  assert.equal(second.root.querySelector('input[type="url"]'), null);
  assert.equal(second.root.querySelector('button[data-merge-strategy="merge"]')?.getAttribute("aria-pressed"), "true");
  second.dispose();

  const third = mountPanel();
  await third.answer("github.status", status());
  assert.equal(third.root.querySelector('input[type="url"]'), null, "a closed form stays closed");
});

test("the merge section offers the repository's strategies only for an open pull request", async () => {
  const panel = mountPanel();
  const draft = status();
  draft.details[1] = { ...draft.details[1], state: "draft" };
  draft.pull_requests[1] = { ...draft.pull_requests[1], state: "draft" };
  await panel.answer("github.status", draft);
  const strategies = () => [...panel.root.querySelectorAll("button[data-merge-strategy]")].map((button) => button.textContent);
  assert.deepEqual(strategies(), ["squash", "merge"]);
  list(panel.root).focus();
  press("j");
  assert.deepEqual(strategies(), [], "a draft cannot be merged");
});

test("the footer hints the shortcuts for the highlighted pull request", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  const hints = () => [...panel.root.querySelectorAll(".footer .hints span")].map((hint) => hint.textContent);
  assert.deepEqual(hints(), ["↑↓ select", "O open", "S strategy", "R refresh", "L link"]);
  list(panel.root).focus();
  press("j");
  assert.deepEqual(hints(), ["↑↓ select", "O open", "U unlink", "S strategy", "R refresh", "L link"]);
});

test("a refresh that changed stored pull requests tells the host once", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", { ...status(), changed: true });
  assert.equal(panel.changes(), 1);
  const unchanged = mountPanel("s2");
  await unchanged.answer("github.status", status());
  assert.equal(unchanged.changes(), 0);
});

const withoutBranchPullRequest = () => ({ ...status(), pull_requests: [pr(2, "manual")], own_url: url(2), details: [detail(2)] });
const actionLabels = (root: Element) =>
  [...root.querySelectorAll(".form-actions button")].map((button) => button.firstChild?.textContent?.trim());

test("Create PR shows beside Link only while the branch has no live pull request", async () => {
  const withBranch = mountPanel();
  await withBranch.answer("github.status", status());
  assert.deepEqual(actionLabels(withBranch.root), ["Link pull request"]);
  const withoutBranch = mountPanel("s2");
  await withoutBranch.answer("github.status", withoutBranchPullRequest());
  assert.deepEqual(actionLabels(withoutBranch.root), ["Create PR", "Link pull request"]);
});

test("Escape closes the create form for good", async () => {
  const first = mountPanel();
  await first.answer("github.status", withoutBranchPullRequest());
  press("c", document.body);
  await first.answer("github.defaults", { title: "Title", body: "", base_branch: "main" });
  const title = first.root.querySelector<HTMLInputElement>("form input");
  assert.equal(title?.value, "Title");
  press("Escape", title);
  assert.equal(first.root.querySelector("form"), null);
  first.dispose();
  const second = mountPanel();
  await second.answer("github.status", withoutBranchPullRequest());
  assert.equal(second.root.querySelector("form"), null);
});

test("Cmd or Ctrl+Enter submits a form", async () => {
  for (const modifier of [{ metaKey: true }, { ctrlKey: true }]) {
    const panel = mountPanel(`s-${Object.keys(modifier)[0]}`);
    await panel.answer("github.status", status());
    press("l", document.body);
    const field = panel.root.querySelector<HTMLInputElement>('input[type="url"]');
    field.value = "https://github.com/o/r/pull/9";
    press("Enter", field, modifier);
    assert.deepEqual(panel.pending("github.link").map((call) => call.params.url), ["https://github.com/o/r/pull/9"]);
  }
});
