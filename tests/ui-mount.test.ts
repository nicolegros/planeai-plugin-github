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
function mountPanel(sessionId = "s1", { listens = true, echoes = true, changedFails = false } = {}) {
  const calls: HostCall[] = [];
  let changed = 0;
  const listeners = new Set<() => void>();
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
      recipient: { getFocusedAgentSession: async () => ({ id: "agent" }) },
      // Like the host, data.changed notifies every view of the plugin, this one included, later
      // on, as a Tauri event does.
      data: {
        changed: async () => {
          changed += 1;
          if (changedFails) throw new Error("the host could not refresh plugin views");
          if (echoes) setTimeout(() => listeners.forEach((listener) => listener()), 0);
        },
        ...(listens ? { onChanged: (listener: () => void) => { listeners.add(listener); return () => listeners.delete(listener); } } : {}),
        notify: () => {},
      },
    },
  });
  const pending = (method: string) => calls.filter((call) => call.method === method);
  return {
    root, dispose, calls, pending,
    changes: () => changed,
    /** Another view of the plugin changed its data. */
    dataChangedElsewhere: () => listeners.forEach((listener) => listener()),
    listening: () => listeners.size,
    async fail(method: string, error: string) {
      const call = pending(method).find((candidate) => !candidate.answered);
      call.answered = true;
      call.reply.reject(new Error(error));
      await settle();
    },
    async answer(method: string, value: unknown) {
      const call = pending(method).find((candidate) => !candidate.answered);
      call.answered = true;
      call.reply.resolve(value);
      await settle();
    },
  };
}

const withoutBranchPullRequest = () => ({ ...status(), pull_requests: [pr(2, "manual")], own_url: url(2), details: [detail(2)] });
const actionLabels = (root: Element) =>
  [...root.querySelectorAll(".form-actions button")].map((button) => button.firstChild?.textContent?.trim());
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

/** Unlinks #2 and waits for the host's echo, returning the panel's own reload and the echo's. */
async function unlinkAndEcho(panel: ReturnType<typeof mountPanel>) {
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  await settle();
  assert.equal(panel.changes(), 1);
  const [, own, echoed] = panel.pending("github.status");
  assert.ok(own && echoed, "the panel reloads itself, and the echo reloads it once more");
  return { own, echoed };
}

const title = (root: Element) => root.querySelector(".pr-title").textContent;
const idle = (root: Element) => root.querySelector<HTMLButtonElement>("button.refresh").disabled === false;

test("after a change, an echo answer landing first shows the newest list and ends the wait", async () => {
  const panel = mountPanel();
  const { own, echoed } = await unlinkAndEcho(panel);
  echoed.reply.resolve(status("Echo"));
  await settle();
  assert.match(title(panel.root), /^Echo/);
  assert.ok(idle(panel.root));
  own.reply.resolve(status("Own"));
  await settle();
  assert.match(title(panel.root), /^Echo/, "the older answer is dropped");
  assert.ok(idle(panel.root));
});

test("after a change, its own answer landing first waits for the newer echo answer", async () => {
  const panel = mountPanel();
  const { own, echoed } = await unlinkAndEcho(panel);
  own.reply.resolve(status("Own"));
  await settle();
  assert.ok(!idle(panel.root), "a newer reload is still on its way");
  echoed.reply.resolve(status("Echo"));
  await settle();
  assert.match(title(panel.root), /^Echo/);
  assert.ok(idle(panel.root));
  panel.dataChangedElsewhere();
  assert.equal(panel.pending("github.status").length, 4, "a change elsewhere right after still reloads");
});
test("a host whose echo never arrives still sees the panel return to idle with the fresh list", async () => {
  const panel = mountPanel("s1", { echoes: false });
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  assert.equal(panel.pending("github.status").length, 2);
  await panel.answer("github.status", status("Fresh"));
  assert.match(panel.root.querySelector(".pr-title").textContent, /^Fresh/);
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, false);
});

test("an older status answer landing after a change neither ends the wait nor shows the old list", async () => {
  const panel = mountPanel("s1", { echoes: false });
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>("button.refresh").click();
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  const [, older, own] = panel.pending("github.status");
  older.reply.resolve(status("Old"));
  await settle();
  assert.doesNotMatch(panel.root.querySelector(".pr-title").textContent, /^Old/);
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, true, "still waiting for its own reload");
  own.reply.resolve(status("Fresh"));
  await settle();
  assert.match(panel.root.querySelector(".pr-title").textContent, /^Fresh/);
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, false);
});
test("without data change notifications, a change reloads the panel itself", async () => {
  const panel = mountPanel("s1", { listens: false });
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  assert.equal(panel.pending("github.status").length, 2);
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

test("a data change elsewhere refreshes in place, keeping the highlight, the open form, and focus", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  list(panel.root).focus();
  press("j");
  panel.root.querySelector<HTMLButtonElement>('button[data-merge-strategy="merge"]').click();
  press("l", document.body);
  const field = panel.root.querySelector<HTMLInputElement>('input[type="url"]');
  field.focus();
  field.value = "https://github.com/o/r/pull/9";
  field.dispatchEvent(new window.Event("input", { bubbles: true }));
  assert.deepEqual(
    [...panel.root.querySelectorAll(".footer .hints span")].map((hint) => hint.textContent),
    ["L link", "Esc close"],
    "the link form documents its keyboard flow"
  );

  panel.dataChangedElsewhere();
  assert.equal(panel.pending("github.status").length, 2);
  await panel.answer("github.status", status("Fresh"));
  assert.equal(document.activeElement, field, "typing goes on in the same field");
  assert.equal(field.value, "https://github.com/o/r/pull/9");

  press("Escape", field);
  assert.equal(selectedNumber(panel.root), "#2");
  assert.match(panel.root.querySelector('.pr-row[aria-selected="true"] .pr-title').textContent, /^Fresh/);
  assert.equal(panel.root.querySelector('button[data-merge-strategy="merge"]')?.getAttribute("aria-pressed"), "true");
  panel.dispose();
  assert.equal(panel.listening(), 0, "the panel stops listening once disposed");
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

test("a refresh that changed stored pull requests tells the host, and the echo settles", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", { ...status(), changed: true });
  await settle();
  assert.equal(panel.changes(), 1);
  assert.equal(panel.pending("github.status").length, 2, "the host's echo reloads once");
  await panel.answer("github.status", status());
  assert.equal(panel.changes(), 1, "an unchanged refresh tells nobody");
  assert.equal(panel.pending("github.status").length, 2, "and nothing loops");
});

test("Create PR shows beside Link only while the branch has no live pull request", async () => {
  const withBranch = mountPanel();
  await withBranch.answer("github.status", status());
  assert.deepEqual(actionLabels(withBranch.root), ["Link pull request"]);
  const withoutBranch = mountPanel("s2");
  await withoutBranch.answer("github.status", withoutBranchPullRequest());
  assert.deepEqual(actionLabels(withoutBranch.root), ["Create PR", "Link pull request"]);
});

test("Escape closes the create form", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", withoutBranchPullRequest());
  press("c", document.body);
  await panel.answer("github.defaults", { title: "Title", body: "", base_branch: "main" });
  const title = panel.root.querySelector<HTMLInputElement>("form input");
  assert.equal(title?.value, "Title");
  press("Escape", title);
  assert.equal(panel.root.querySelector("form"), null);
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


test("a refresh that fails while a form is open shows its error once the list is back", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  press("l", document.body);
  panel.dataChangedElsewhere();
  await panel.fail("github.status", "GitHub is down");
  assert.ok(panel.root.querySelector('input[type="url"]'), "the form stays");
  press("Escape", panel.root.querySelector('input[type="url"]'));
  assert.match(panel.root.querySelector('[role="alert"]')?.textContent ?? "", /GitHub is down/);
});

test("after a change, the panel stays inert until its reload lands", async () => {
  const panel = mountPanel("s1", { echoes: false });
  await panel.answer("github.status", withoutBranchPullRequest());
  press("c", document.body);
  await panel.answer("github.defaults", { title: "Title", body: "", base_branch: "main" });
  [...panel.root.querySelectorAll<HTMLButtonElement>("form button")].find((button) => button.textContent.startsWith("Create")).click();
  await panel.answer("github.create", { url: url(7) });
  assert.equal(panel.pending("github.status").length, 2, "the panel reloads itself");
  assert.match(panel.root.textContent, /Refreshing GitHub status/);
  assert.deepEqual([...panel.root.querySelectorAll("button")].filter((button) => !button.disabled).map((button) => button.textContent), [], "no action can start on the stale list");
  press("l", document.body);
  press("c", document.body);
  assert.equal(panel.root.querySelector("form"), null, "shortcuts do nothing meanwhile");
  assert.equal(panel.pending("github.defaults").length, 1);
  await panel.answer("github.status", status());
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, false);
  assert.doesNotMatch(panel.root.textContent, /Refreshing GitHub status/);
});

test("sending CI failures holds the panel the same way, without reloading it", async () => {
  const panel = mountPanel();
  const failing = status();
  failing.details[0] = { ...failing.details[0], checks: [{ name: "test", workflowName: "CI", result: "fail" }] } as never;
  await panel.answer("github.status", failing);
  panel.root.querySelector<HTMLButtonElement>('button[data-shortcut="f"]').click();
  await settle();
  assert.deepEqual(
    panel.pending("github.sendFailureLogs").map((call) => call.params),
    [{ session_id: "s1", recipient_session_id: "agent", url: url(1) }],
    "the highlighted pull request's failures go to the focused agent"
  );
  assert.deepEqual([...panel.root.querySelectorAll("button")].filter((button) => !button.disabled).map((button) => button.textContent), []);
  await panel.answer("github.sendFailureLogs", { sent: true });
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, false);
  assert.equal(panel.pending("github.status").length, 1);
  assert.equal(panel.changes(), 0);
});
test("a host that rejects the change notice does not stop the panel", async () => {
  const panel = mountPanel("s1", { changedFails: true });
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  await panel.answer("github.status", status("Fresh"));
  assert.match(panel.root.querySelector(".pr-title").textContent, /^Fresh/);
  assert.equal(panel.root.querySelector<HTMLButtonElement>("button.refresh").disabled, false);
});
