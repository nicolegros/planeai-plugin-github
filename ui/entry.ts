import {
  CHECK_LABELS,
  SOURCE_LABELS,
  STATE_LABELS,
  checkTone,
  currentPullRequest,
  displayOrder,
  foreignRepository,
  isLive,
  moveSelection,
  resolveSelection,
  stateTone,
  type PullRequest,
} from "./pull-requests.js";

type GithubPluginContext = {
  session?: { id?: string; name?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<any>;
    navigation: { openExternal: (url: string) => void };
    recipient: {
      getFocusedAgentSession: () => Promise<{ id: string } | null>;
    };
    data: { changed?: () => Promise<unknown>; notify: (message: string, kind?: "success" | "error") => void };
  };
};

type GithubPluginEntrypoint = {
  mount: (root: HTMLElement, context: GithubPluginContext) => () => void;
};

const githubEntrypoint: GithubPluginEntrypoint = {
  mount(root, context) {
    const sourceSessionId = context.session?.id;
    let snapshot = null;
    let defaults = null;
    let creating = false;
    let linking = false;
    let busy = false;
    let disposed = false;
    let selectedStrategy = "squash";
    // The highlighted pull request; the header, checks, merge, and failure hand-off follow it.
    let selectedUrl: string | null = null;

    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        main { height:100%; overflow:auto; padding:0 var(--planeai-space-5) var(--planeai-space-5); }
        .header { display:flex; align-items:center; gap:var(--planeai-space-2); padding:0 0 var(--planeai-space-3); border-bottom:1px solid var(--planeai-border); }
        .pr-link { min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; min-height:0; border:0; padding:0; border-radius:0; background:none; color:var(--planeai-accent); font:500 13px var(--planeai-font-mono); text-align:left; }
        .pr-link:hover:not(:disabled) { background:none; text-decoration:underline; }
        .state { margin-left:auto; flex:none; color:var(--tone, var(--planeai-text-muted)); font:10px var(--planeai-font-mono); letter-spacing:.06em; text-transform:uppercase; }
        [data-tone="success"] { --tone:var(--planeai-success); }
        [data-tone="warning"] { --tone:var(--planeai-warning); }
        [data-tone="danger"] { --tone:var(--planeai-danger); }
        [data-tone="info"] { --tone:var(--planeai-info); }
        [data-tone="muted"] { --tone:var(--planeai-text-muted); }
        .section { padding:var(--planeai-space-3) 0; border-bottom:1px solid var(--planeai-border); }
        .section:last-of-type { border-bottom:0; }
        .section-title { color:var(--planeai-text-subtle); font:600 10px var(--planeai-font-sans); letter-spacing:.06em; text-transform:uppercase; }
        .row { display:flex; align-items:center; gap:var(--planeai-space-2); }
        .muted { color:var(--planeai-text-muted); }
        .status, .error { margin:var(--planeai-space-2) 0 0; font-size:11px; }
        .error { color:var(--planeai-danger); white-space:pre-wrap; }
        .draft { width:100%; min-height:30px; padding:6px 8px; border-color:color-mix(in srgb, var(--planeai-success) 40%, var(--planeai-border)); background:color-mix(in srgb, var(--planeai-success) 10%, transparent); color:var(--planeai-success); font-size:12px; font-weight:500; }
        .checks-heading { display:flex; align-items:center; margin-bottom:var(--planeai-space-2); }
        .refresh { min-height:0; margin-left:auto; padding:0; border:0; border-radius:0; background:transparent; color:var(--planeai-text-subtle); font-size:14px; line-height:14px; }
        .refresh:hover:not(:disabled) { background:transparent; color:var(--planeai-text-muted); }
        .passed { margin-left:var(--planeai-space-2); color:var(--tone); font:10px var(--planeai-font-mono); }
        .pr-list { display:grid; margin:0 calc(-1 * var(--planeai-space-2)); }
        .pr-row { display:grid; grid-template-columns:8px 42px minmax(0, 1fr) 12px 52px 92px; align-items:center; column-gap:var(--planeai-space-2); min-height:32px; padding:4px var(--planeai-space-2); border-radius:6px; }
        .pr-list:focus { outline:none; }
        .pr-row { cursor:default; }
        .pr-row:hover { background:var(--planeai-accent-subtle); }
        .pr-row[aria-selected="true"] { background:color-mix(in srgb, var(--planeai-accent) 9%, transparent); box-shadow:inset 0 0 0 1px color-mix(in srgb, var(--planeai-accent) 22%, transparent); }
        .pr-list:focus-visible .pr-row[aria-selected="true"] { box-shadow:inset 0 0 0 2px var(--planeai-accent); }
        .pr-row[data-current] .pr-number { font-weight:600; }
        .pr-dot { width:8px; height:8px; border-radius:999px; background:var(--tone); }
        .pr-dot[data-state="draft"] { background:transparent; box-shadow:inset 0 0 0 1.5px var(--tone); }
        .pr-number { color:var(--tone); font:500 11.5px var(--planeai-font-mono); }
        .pr-text { display:flex; align-items:baseline; gap:6px; min-width:0; }
        .pr-title { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:var(--planeai-text); font-size:12px; }
        .pr-repo { flex:none; color:var(--planeai-text-subtle); font:10.5px var(--planeai-font-mono); }
        .pr-source { color:var(--planeai-text-subtle); font:10px var(--planeai-font-mono); white-space:nowrap; }
        .pr-ci { min-width:12px; color:var(--tone); font-size:11px; line-height:1; text-align:center; }
        .pr-actions { display:flex; align-items:center; justify-content:flex-end; gap:2px; }
        .pr-actions button { min-height:22px; padding:0 6px; border:0; border-radius:5px; background:transparent; color:var(--planeai-text-muted); font-size:11px; }
        .pr-actions button:hover:not(:disabled) { background:var(--planeai-surface-raised); color:var(--planeai-text); }
        .check-list { display:grid; gap:6px; }
        .check { display:flex; align-items:center; gap:var(--planeai-space-2); min-width:0; }
        .check .index, kbd { border:1px solid var(--planeai-border); border-radius:4px; padding:0 4px; color:var(--planeai-text-subtle); font:10px var(--planeai-font-mono); }
        .check .icon { width:14px; text-align:center; }
        .check .name { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:var(--planeai-text-muted); font:11.5px var(--planeai-font-mono); }
        .pass { color:var(--planeai-success); } .fail { color:var(--planeai-danger); } .pending { color:var(--planeai-warning); }
        .failure { width:100%; min-height:30px; margin-top:var(--planeai-space-2); padding:6px 8px; border:0; background:color-mix(in srgb, var(--planeai-danger) 10%, transparent); color:var(--planeai-danger); font-size:12px; }
        .warning { margin:0 calc(-1 * var(--planeai-space-5)); padding:var(--planeai-space-3) var(--planeai-space-5); background:color-mix(in srgb, var(--planeai-warning) 8%, transparent); }
        .warning.error { background:color-mix(in srgb, var(--planeai-danger) 8%, transparent); }
        .warning-title { color:var(--planeai-warning); font-size:11px; font-weight:500; }
        .warning.error .warning-title { color:var(--planeai-danger); }
        .warning p { margin:4px 0 0 18px; color:var(--planeai-text-muted); font-size:11px; }
        .strategies { display:flex; gap:6px; margin:var(--planeai-space-2) 0; }
        .strategies button { flex:1; min-height:29px; padding:4px 6px; border-radius:7px; font-size:11.5px; font-weight:500; }
        .strategies button[aria-pressed="true"] { border-color:var(--planeai-accent); background:var(--planeai-accent); color:var(--planeai-on-accent); }
        .strategies button:focus-visible { outline:2px solid var(--planeai-accent); outline-offset:1px; }
        .merge { width:100%; min-height:34px; border-color:var(--planeai-accent); background:var(--planeai-accent); color:var(--planeai-on-accent); font-size:12.5px; font-weight:500; }
        .merge:disabled { border-color:var(--planeai-border); background:var(--planeai-surface-raised); color:var(--planeai-text-muted); }
        .shortcut-help { margin:0; color:var(--planeai-text-subtle); font-size:10px; }
        .footer { display:flex; align-items:center; justify-content:space-between; gap:var(--planeai-space-3); padding-top:var(--planeai-space-2); color:var(--planeai-text-subtle); font:10px var(--planeai-font-mono); }
        .footer .hints { display:flex; gap:var(--planeai-space-3); }
        label { display:grid; gap:6px; font-weight:600; }
        textarea { width:100%; resize:vertical; }
        .form-actions { display:flex; gap:var(--planeai-space-2); padding-top:var(--planeai-space-2); border-top:1px solid var(--planeai-border); }
        .section > .form-actions:first-child { padding-top:0; border-top:0; }
      </style>
      <div data-content></div>`;
    root.replaceChildren(page);

    const content = page.querySelector("[data-content]");
    const reportContentHeight = () => {
      const height = Math.ceil(page.scrollHeight);
      if (height > 0) window.parent.postMessage({ type: "content-height", height }, "*");
    };
    const contentObserver = new MutationObserver(() => requestAnimationFrame(reportContentHeight));
    contentObserver.observe(content, { childList: true, subtree: true, characterData: true });
    const call = (method, params = {}) => context.host.call(method, { session_id: sourceSessionId, ...params });
    const editableTarget = (target) => target instanceof Element && target.closest("input, textarea, select, [contenteditable='true']");
    // Refreshes every sidebar pill; the host also remounts this panel.
    const dataChanged = () => { void context.host.data.changed?.(); };

    const stateName = (pr) => pr?.state || "open";
    const isMerged = (pr) => stateName(pr) === "merged";
    const isDraft = (pr) => stateName(pr) === "draft";
    const canMerge = (pr) => !busy && !pr.conflicting && !pr.merge_blocked && stateName(pr) === "open";
    const classifyCheck = (check) => {
      const value = String(check.conclusion || check.status || "").toUpperCase();
      if (["SUCCESS", "PASSED", "PASS", "NEUTRAL", "SKIPPED"].includes(value)) return "pass";
      if (["FAILURE", "FAILED", "CANCELLED", "TIMED_OUT", "ERROR", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"].includes(value)) return "fail";
      return "pending";
    };
    const pullRequests = (): PullRequest[] => (Array.isArray(snapshot?.pull_requests) ? snapshot.pull_requests : []);
    const details = () => (Array.isArray(snapshot?.details) ? snapshot.details : []);
    const selection = () => resolveSelection(pullRequests(), selectedUrl, snapshot?.own_url ?? null);
    const detailOf = (pr: PullRequest | null) => (pr ? details().find((detail) => detail.url === pr.url) ?? null : null);
    const branchHasPullRequest = () =>
      pullRequests().some((pr) => isLive(pr) && pr.head_branch === snapshot?.branch && !foreignRepository(pr, snapshot?.repository));

    function button(label, handler, { disabled = false, shortcut = null, className = "" } = {}) {
      const element = document.createElement("button");
      element.type = "button";
      element.className = className;
      element.textContent = label;
      if (shortcut) {
        element.dataset.shortcut = shortcut;
        const hint = document.createElement("kbd");
        hint.textContent = shortcut === "shift+r" ? "⇧R" : shortcut.toUpperCase();
        element.append(" ", hint);
      }
      element.disabled = disabled || busy;
      element.addEventListener("click", handler);
      return element;
    }

    async function mutate(action: () => Promise<unknown>, success = "") {
      busy = true; render();
      try {
        await action();
        busy = false;
        if (success) context.host.data.notify(success, "success");
        dataChanged();
        await load();
      } catch (error) { busy = false; render(String(error)); }
    }

    function installFormKeyboard(form, focusField, cancel) {
      form.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          cancel();
        } else if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          event.preventDefault();
          form.requestSubmit();
        }
      });
      requestAnimationFrame(() => focusField.focus());
    }

    function appendHeader(current: PullRequest | null) {
      const header = document.createElement("div");
      header.className = "header";
      const name = context.session?.name || "Pull requests";
      const open = button(current ? `${name} · #${current.number} ↗` : name, () => context.host.navigation.openExternal(current.url), { shortcut: "o", className: "pr-link" });
      open.disabled = !current;
      header.append(open);
      if (current) {
        const state = document.createElement("span");
        state.className = "state";
        state.dataset.tone = stateTone(current.state);
        state.textContent = STATE_LABELS[current.state] ?? current.state;
        header.append(state);
      }
      content.append(header);
    }

    function appendStatus(message, error = "") {
      const status = document.createElement("p");
      status.className = "status muted";
      status.setAttribute("role", "status");
      status.textContent = message;
      content.append(status);
      if (error) {
        const detail = document.createElement("p");
        detail.className = "error";
        detail.setAttribute("role", "alert");
        detail.textContent = error;
        content.append(detail);
      }
    }

    function appendFooter(items) {
      const footer = document.createElement("div");
      footer.className = "footer";
      const normal = document.createElement("span");
      normal.textContent = "NORMAL";
      const hints = document.createElement("div");
      hints.className = "hints";
      hints.innerHTML = items.map(([key, label]) => `<span><kbd>${key}</kbd> ${label}</span>`).join("");
      footer.append(normal, hints);
      content.append(footer);
    }

    function renderSetup(message) {
      appendHeader(null);
      appendStatus(message);
      const section = document.createElement("section");
      section.className = "section";
      section.innerHTML = `<p class="muted">Install GitHub CLI and authenticate with: <code>gh auth login</code></p>`;
      content.append(section);
    }

    function renderCreate() {
      appendHeader(null);
      const section = document.createElement("section");
      section.className = "section";
      const title = document.createElement("div"); title.className = "section-title"; title.textContent = "Create pull request";
      const form = document.createElement("form");
      const titleField = document.createElement("input"); titleField.value = defaults?.title || ""; titleField.required = true;
      const bodyField = document.createElement("textarea"); bodyField.rows = 10; bodyField.value = defaults?.body || "";
      const baseField = document.createElement("input"); baseField.value = defaults?.base_branch || "main"; baseField.required = true;
      const draft = document.createElement("input"); draft.type = "checkbox";
      const labelled = (label, field) => { const wrapper = document.createElement("label"); wrapper.textContent = label; wrapper.append(field); return wrapper; };
      const draftLabel = document.createElement("label"); draftLabel.className = "row"; draftLabel.append(draft, document.createTextNode("Draft pull request"));
      const actions = document.createElement("div"); actions.className = "form-actions";
      actions.append(
        button("Cancel", () => { creating = false; render(); }),
        button("Create", () => {
          void mutate(async () => { await call("github.create", { title: titleField.value, body: bodyField.value, base_branch: baseField.value, draft: draft.checked }); creating = false; });
        }, { shortcut: "c" }),
      );
      form.addEventListener("submit", (event) => { event.preventDefault(); actions.querySelectorAll("button")[1]?.click(); });
      installFormKeyboard(form, titleField, () => { creating = false; render(); });
      form.append(labelled("Title", titleField), labelled("Body", bodyField), labelled("Base branch", baseField), draftLabel, actions);
      section.append(title, form);
      content.append(section);
      appendFooter([["C", "create"], ["Esc", "close"]]);
    }

    function renderLink() {
      appendHeader(null);
      const section = document.createElement("section");
      section.className = "section";
      const title = document.createElement("div"); title.className = "section-title"; title.textContent = "Link pull request";
      const form = document.createElement("form");
      const urlField = document.createElement("input"); urlField.type = "url"; urlField.required = true; urlField.placeholder = "https://github.com/owner/repo/pull/123";
      const labelled = document.createElement("label"); labelled.textContent = "Pull request URL"; labelled.append(urlField);
      const actions = document.createElement("div"); actions.className = "form-actions";
      actions.append(
        button("Cancel", () => { linking = false; render(); }),
        button("Link pull request", () => {
          void mutate(async () => { await call("github.link", { url: urlField.value }); linking = false; }, "Pull request linked");
        }, { shortcut: "l" }),
      );
      form.addEventListener("submit", (event) => { event.preventDefault(); actions.querySelectorAll("button")[1]?.click(); });
      installFormKeyboard(form, urlField, () => { linking = false; render(); });
      form.append(labelled, actions); section.append(title, form); content.append(section);
      appendFooter([["L", "link"], ["Esc", "close"]]);
    }

    function pullRequestRow(pr: PullRequest, current: PullRequest | null, selected: PullRequest | null) {
      const row = document.createElement("div");
      row.className = "pr-row";
      row.id = `pull-request-${pr.number}-${pr.repository.replace(/[^a-z0-9]/gi, "-")}`;
      row.setAttribute("role", "option");
      const isSelected = selected?.url === pr.url;
      row.setAttribute("aria-selected", String(isSelected));
      row.dataset.tone = stateTone(pr.state);
      if (current && pr.url === current.url) row.dataset.current = "";
      row.addEventListener("click", (event) => {
        if (event.target instanceof Element && event.target.closest("button")) return;
        select(pr.url);
      });
      const dot = document.createElement("span"); dot.className = "pr-dot"; dot.dataset.state = pr.state;
      dot.setAttribute("role", "img"); dot.setAttribute("aria-label", STATE_LABELS[pr.state] ?? pr.state);
      const number = document.createElement("span"); number.className = "pr-number"; number.textContent = `#${pr.number}`;
      const text = document.createElement("span"); text.className = "pr-text";
      const title = document.createElement("span"); title.className = "pr-title"; title.textContent = pr.title || pr.url; title.title = pr.title || pr.url;
      text.append(title);
      const repository = foreignRepository(pr, snapshot?.repository);
      if (repository) { const repo = document.createElement("span"); repo.className = "pr-repo"; repo.textContent = repository; text.append(repo); }
      const ci = document.createElement("span"); ci.className = "pr-ci";
      const ciTone = isLive(pr) ? checkTone(pr.checks) : null;
      if (ciTone) {
        ci.dataset.tone = ciTone;
        ci.textContent = pr.checks === "passing" ? "✓" : pr.checks === "failing" ? "✗" : "●";
        ci.setAttribute("role", "img"); ci.setAttribute("aria-label", CHECK_LABELS[pr.checks]);
      }
      const source = document.createElement("span"); source.className = "pr-source"; source.textContent = SOURCE_LABELS[pr.source] ?? pr.source;
      const actions = document.createElement("span"); actions.className = "pr-actions";
      const open = button("↗", () => context.host.navigation.openExternal(pr.url));
      open.setAttribute("aria-label", `Open #${pr.number} on GitHub`); open.title = "Open on GitHub";
      if (pr.source !== "branch") {
        const label = pr.source === "stack" ? "Dismiss" : "Unlink";
        const remove = button(label, () => { void mutate(() => call("github.unlink", { url: pr.url })); });
        remove.setAttribute("aria-label", `${label} #${pr.number}`);
        // U acts on the highlighted row; the footer hints it, so the row stays uncluttered.
        if (isSelected) remove.dataset.shortcut = "u";
        actions.append(remove);
      }
      actions.append(open);
      row.append(dot, number, text, ci, source, actions);
      return row;
    }

    function appendPullRequests(prs: PullRequest[], current: PullRequest | null, selected: PullRequest | null) {
      const section = document.createElement("section"); section.className = "section";
      const heading = document.createElement("div"); heading.className = "checks-heading";
      const label = document.createElement("span"); label.className = "section-title"; label.textContent = prs.length ? `Pull requests · ${prs.length}` : "No pull request";
      const refresh = button("↻", () => { void load(); }, { shortcut: "r", className: "refresh" });
      refresh.setAttribute("aria-label", "Refresh pull requests");
      heading.append(label, refresh);
      section.append(heading);
      if (prs.length) {
        const list = document.createElement("div"); list.className = "pr-list";
        list.setAttribute("role", "listbox"); list.setAttribute("aria-label", "Pull requests"); list.tabIndex = 0;
        for (const pr of displayOrder(prs)) list.append(pullRequestRow(pr, current, selected));
        const active = list.querySelector('[aria-selected="true"]');
        if (active) list.setAttribute("aria-activedescendant", active.id);
        section.append(list);
      } else {
        const hint = document.createElement("p"); hint.className = "muted"; hint.textContent = "Create a pull request for the session branch, or link an existing one.";
        section.append(hint);
      }
      content.append(section);
    }

    function appendActions() {
      const section = document.createElement("section"); section.className = "section";
      const actions = document.createElement("div"); actions.className = "form-actions";
      if (!branchHasPullRequest()) {
        actions.append(button("Create PR", async () => { try { defaults = await call("github.defaults"); creating = true; render(); } catch (loadError) { render(String(loadError)); } }, { shortcut: "c" }));
      }
      actions.append(button("Link pull request", () => { linking = true; render(); }, { shortcut: "l" }));
      section.append(actions);
      content.append(section);
    }

    function appendChecks(pr) {
      const checks = Array.isArray(pr.checks) ? pr.checks : [];
      if (!checks.length) return;
      const section = document.createElement("section"); section.className = "section";
      const heading = document.createElement("div"); heading.className = "checks-heading";
      const label = document.createElement("span"); label.className = "section-title"; label.textContent = `Checks · #${pr.number}`;
      heading.append(label);
      for (const [result, word, tone] of [["fail", "failed", "danger"], ["pending", "pending", "warning"], ["pass", "passed", "success"]]) {
        const count = checks.filter((check) => classifyCheck(check) === result).length;
        if (!count) continue;
        const summary = document.createElement("span"); summary.className = "passed"; summary.dataset.tone = tone; summary.textContent = `${count} ${word}`;
        heading.append(summary);
      }
      const list = document.createElement("div"); list.className = "check-list";
      for (const [index, check] of checks.entries()) {
        const result = classifyCheck(check);
        const row = document.createElement("div"); row.className = "check";
        const number = document.createElement("span"); number.className = "index"; number.textContent = String(index + 1);
        const icon = document.createElement("span"); icon.className = `icon ${result}`; icon.textContent = result === "pass" ? "✓" : result === "fail" ? "✗" : "◌";
        const name = document.createElement("span"); name.className = "name"; name.textContent = check.workflowName ? `${check.workflowName} / ${check.name}` : check.name || "Unnamed check";
        row.append(number, icon, name); list.append(row);
      }
      section.append(heading, list); content.append(section);
    }

    function appendWarning(className, title, detail) {
      const warning = document.createElement("section"); warning.className = `warning ${className}`;
      const heading = document.createElement("div"); heading.className = "row warning-title"; heading.textContent = title;
      warning.append(heading);
      if (detail) { const text = document.createElement("p"); text.textContent = detail; warning.append(text); }
      content.append(warning);
    }

    function selectMergeStrategy(strategy) {
      selectedStrategy = strategy;
      for (const button of content.querySelectorAll<HTMLButtonElement>("button[data-merge-strategy]")) {
        button.setAttribute("aria-pressed", String(button.dataset.mergeStrategy === strategy));
      }
      const merge = content.querySelector("button[data-merge-confirm]");
      if (merge) merge.textContent = `Merge #${detailOf(selection())?.number} with ${strategy}`;
    }

    function cycleMergeStrategy() {
      const choices = Array.from(content.querySelectorAll<HTMLButtonElement>("button[data-merge-strategy]")).filter((element) => !element.disabled);
      if (!choices.length) return false;
      const active = choices.findIndex((choice) => choice === document.activeElement);
      const selected = choices.findIndex((element) => element.dataset.mergeStrategy === selectedStrategy);
      const next = choices[(active >= 0 ? active + 1 : selected + 1) % choices.length];
      selectMergeStrategy(next.dataset.mergeStrategy);
      next.focus();
      return true;
    }

    async function mergeSelected(pr) {
      if (!canMerge(pr)) return;
      await mutate(() => call("github.merge", { strategy: selectedStrategy, url: pr.url }));
    }

    function confirmFocusedMergeStrategy() {
      const focused = document.activeElement;
      if (!(focused instanceof HTMLButtonElement) || !focused.matches("button[data-merge-strategy]")) return false;
      const confirm = content.querySelector("button[data-merge-confirm]");
      if (!(confirm instanceof HTMLButtonElement) || confirm.disabled) return false;
      confirm.click();
      return true;
    }

    /** Checks, merge, and CI hand-off for the highlighted pull request. */
    function appendDetail(pr) {
      const checks = Array.isArray(pr.checks) ? pr.checks : [];
      const failed = checks.some((check) => classifyCheck(check) === "fail");
      appendChecks(pr);
      if (pr.conflicting && !isMerged(pr)) appendWarning("", `⚠ #${pr.number} has merge conflicts`, "Resolve conflicts on GitHub or rebase locally before merging.");
      if (pr.merge_blocked && !isMerged(pr)) appendWarning("error", "⚠ Merge blocked", "Merge is blocked by repository rules, reviews, or checks.");

      if (isDraft(pr)) {
        const section = document.createElement("section"); section.className = "section";
        section.append(button(`Mark #${pr.number} as ready`, () => { void mutate(() => call("github.markReady", { url: pr.url })); }, { shortcut: "shift+r", className: "draft" }));
        content.append(section);
      }

      const mergeMethods = Array.isArray(pr.merge_methods) ? pr.merge_methods.filter((method) => ["squash", "merge", "rebase"].includes(method)) : [];
      if (!mergeMethods.includes(selectedStrategy)) selectedStrategy = mergeMethods[0] || "squash";
      if (stateName(pr) === "open" && mergeMethods.length) {
        const section = document.createElement("section"); section.className = "section";
        const label = document.createElement("div"); label.className = "section-title"; label.textContent = "Merge";
        const strategies = document.createElement("div"); strategies.className = "strategies";
        for (const strategy of mergeMethods) {
          const choice = button(strategy, () => selectMergeStrategy(strategy), { disabled: !canMerge(pr) });
          choice.dataset.mergeStrategy = strategy;
          choice.setAttribute("aria-pressed", String(strategy === selectedStrategy));
          strategies.append(choice);
        }
        const merge = button(`Merge #${pr.number} with ${selectedStrategy}`, () => { void mergeSelected(pr); }, { disabled: !canMerge(pr), className: "merge" });
        merge.dataset.mergeConfirm = "";
        const help = document.createElement("p"); help.className = "shortcut-help"; help.innerHTML = `<kbd>S</kbd> selects a strategy; <kbd>Enter</kbd> or <kbd>Space</kbd> confirms it.`;
        section.append(label, strategies, merge, help); content.append(section);
      }

      if (failed && !isMerged(pr)) {
        const section = document.createElement("section"); section.className = "section";
        section.append(button("Send failures to agent", async () => { busy = true; render(); try { const recipient = await context.host.recipient.getFocusedAgentSession(); if (!recipient) throw new Error("Focus an agent session before sending CI failures."); await call("github.sendFailureLogs", { recipient_session_id: recipient.id, url: pr.url }); context.host.data.notify("CI failures sent to agent", "success"); } catch (error) { busy = false; render(String(error)); return; } busy = false; render(); }, { shortcut: "f", className: "failure" }));
        content.append(section);
      }
    }

    function renderPullRequests(error = "") {
      const prs = pullRequests();
      const current = currentPullRequest(prs);
      const selected = selection();
      selectedUrl = selected?.url ?? null;
      appendHeader(selected);
      appendPullRequests(prs, current, selected);
      const detail = detailOf(selected);
      if (detail) appendDetail(detail);
      appendActions();
      if (error) appendStatus("GitHub could not complete that action.", error);
      const hints = [["R", "refresh"], ["L", "link"]];
      if (!branchHasPullRequest()) hints.splice(1, 0, ["C", "create"]);
      if (content.querySelector("button[data-merge-strategy]")) hints.unshift(["S", "strategy"]);
      if (selected && selected.source !== "branch") hints.unshift(["U", selected.source === "stack" ? "dismiss" : "unlink"]);
      if (selected) hints.unshift(["O", "open"]);
      if (prs.length > 1) hints.unshift(["↑↓", "select"]);
      appendFooter(hints);
    }

    function render(error = "") {
      content.replaceChildren();
      if (!sourceSessionId) { renderSetup("Select a PlaneAI session before opening this panel."); return; }
      if (!snapshot) { appendHeader(null); appendStatus("Refreshing GitHub status…", error); return; }
      if (!snapshot.applicable) { renderSetup(snapshot.reason || "GitHub is not applicable to this session."); return; }
      if (creating) { renderCreate(); if (error) appendStatus("The pull request could not be created.", error); return; }
      if (linking) { renderLink(); if (error) appendStatus("The pull request could not be linked.", error); return; }
      renderPullRequests(error);
    }

    function select(url: string | null) {
      if (!url || url === selectedUrl) return;
      const listFocused = document.activeElement?.closest?.(".pr-list") != null;
      selectedUrl = url;
      render();
      if (listFocused) content.querySelector<HTMLElement>(".pr-list")?.focus();
    }

    const SELECTION_KEYS: Record<string, 1 | -1> = { ArrowDown: 1, j: 1, ArrowUp: -1, k: -1 };

    function triggerShortcut(shortcut) {
      if (shortcut === "s") return cycleMergeStrategy();
      const action = content.querySelector(`[data-shortcut="${shortcut}"]`);
      if (!(action instanceof HTMLButtonElement) || action.disabled) return false;
      action.click();
      return true;
    }

    function handleKeydown(event) {
      if (disposed || (event.repeat && !SELECTION_KEYS[event.key]) || event.altKey || event.ctrlKey || event.metaKey || editableTarget(event.target)) return;
      if (event.key === "Enter" || event.key === " ") {
        if (!confirmFocusedMergeStrategy()) return;
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      const delta = SELECTION_KEYS[event.key];
      if (delta && !event.shiftKey && snapshot?.applicable && !creating && !linking) {
        select(moveSelection(pullRequests(), selectedUrl, delta)?.url ?? null);
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      const shortcut = event.key === "R" ? "shift+r" : event.key.toLowerCase();
      if (!new Set(["r", "c", "l", "o", "u", "shift+r", "s", "f"]).has(shortcut) || !triggerShortcut(shortcut)) return;
      event.preventDefault();
      event.stopPropagation();
    }

    async function load() {
      if (!sourceSessionId) { render(); return; }
      const previous = snapshot;
      snapshot = null; render();
      try {
        snapshot = await call("github.status");
        if (disposed) return;
        render();
        if (snapshot.changed) dataChanged();
      } catch (error) {
        snapshot = previous?.applicable ? previous : null;
        render(String(error));
      }
    }

    window.addEventListener("keydown", handleKeydown, true);
    void load();
    return () => { disposed = true; contentObserver.disconnect(); window.removeEventListener("keydown", handleKeydown, true); root.replaceChildren(); };
  },
};

export default githubEntrypoint;
