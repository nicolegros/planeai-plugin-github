import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { deferred, installDom, settle, type Deferred } from "./ui-harness.js";
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

/** Mounts the panel with a host whose every call waits until the test answers it. */
function mountPanel(sessionId = "s1") {
  const calls: { method: string; params: Record<string, unknown>; reply: Deferred<unknown> }[] = [];
  let changed = 0;
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = githubEntrypoint.mount(root, {
    session: { id: sessionId, name: "Session" },
    host: {
      call: (method, params) => {
        const reply = deferred<unknown>();
        calls.push({ method, params, reply });
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
      const call = pending(method).find((candidate) => !(candidate as { answered?: boolean }).answered) as { reply: Deferred<unknown>; answered?: boolean };
      call.answered = true;
      call.reply.resolve(value);
      await settle();
    },
  };
}

beforeEach(() => { installDom(); });

test("a change refreshes through the host once, without another status call", async () => {
  const panel = mountPanel();
  await panel.answer("github.status", status());
  panel.root.querySelector<HTMLButtonElement>('button[aria-label="Unlink #2"]').click();
  await panel.answer("github.unlink", { pull_requests: [] });
  assert.equal(panel.changes(), 1);
  assert.equal(panel.pending("github.status").length, 1, "the host remount loads the panel again");
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
