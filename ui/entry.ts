type GithubPluginContext = {
  session?: { id?: string; name?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<any>;
    navigation: { openExternal: (url: string) => void };
  };
};

type GithubPluginEntrypoint = {
  mount: (root: HTMLElement, context: GithubPluginContext) => () => void;
};

const githubEntrypoint: GithubPluginEntrypoint = {
  mount(root, context) {
    const sessionId = context.session?.id;
    let snapshot = null;
    let defaults = null;
    let creating = false;
    let busy = false;
    let disposed = false;
    let selectedStrategy = "squash";

    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        main { height:100%; overflow:auto; padding:0 var(--planeai-space-5) var(--planeai-space-5); }
        .header { display:flex; align-items:center; gap:var(--planeai-space-2); padding:0 0 var(--planeai-space-3); border-bottom:1px solid var(--planeai-border); }
        .pr-link { min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; min-height:0; border:0; padding:0; border-radius:0; background:none; color:var(--planeai-accent); font:500 13px var(--planeai-font-mono); text-align:left; }
        .state { margin-left:auto; color:var(--planeai-success); font:10px var(--planeai-font-mono); letter-spacing:.06em; text-transform:uppercase; }
        .state.merged { color:#bc8cff; }
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
        .passed { margin-left:var(--planeai-space-2); color:var(--planeai-success); font:10px var(--planeai-font-mono); }
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
      </style>
      <div data-content></div>`;
    root.replaceChildren(page);

    const content = page.querySelector("[data-content]");
    const call = (method, params = {}) => context.host.call(method, { session_id: sessionId, ...params });
    const editableTarget = (target) => target instanceof Element && target.closest("input, textarea, select, [contenteditable='true']");

    const stateName = (pr) => pr?.state || "open";
    const isMerged = (pr) => stateName(pr) === "merged";
    const isDraft = (pr) => stateName(pr) === "draft";
    const canMerge = (pr) => !busy && !pr.conflicting && !pr.merge_blocked && !isMerged(pr) && !isDraft(pr);
    const classifyCheck = (check) => {
      const value = String(check.conclusion || check.status || "").toUpperCase();
      if (["SUCCESS", "PASSED", "PASS"].includes(value)) return "pass";
      if (["FAILURE", "FAILED", "CANCELLED", "TIMED_OUT", "ERROR"].includes(value)) return "fail";
      return "pending";
    };

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

    function appendHeader(pr) {
      const header = document.createElement("div");
      header.className = "header";
      const name = context.session?.name || "PR";
      const open = button(pr?.url ? `${name} ↗` : name, () => context.host.navigation.openExternal(pr.url), { shortcut: "o", className: "pr-link" });
      open.disabled = !pr?.url;
      const state = document.createElement("span");
      state.className = `state${isMerged(pr) ? " merged" : ""}`;
      state.textContent = stateName(pr);
      header.append(open, state);
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
        button("Create", async () => {
          busy = true; render();
          try { await call("github.create", { title: titleField.value, body: bodyField.value, base_branch: baseField.value, draft: draft.checked }); creating = false; await load(); }
          catch (error) { render(String(error)); } finally { busy = false; }
        }, { shortcut: "c" }),
      );
      form.addEventListener("submit", (event) => { event.preventDefault(); actions.querySelectorAll("button")[1]?.click(); });
      form.append(labelled("Title", titleField), labelled("Body", bodyField), labelled("Base branch", baseField), draftLabel, actions);
      section.append(title, form);
      content.append(section);
      appendFooter([["C", "create"], ["Esc", "close"]]);
    }

    function appendChecks(checks) {
      if (!checks.length) return;
      const section = document.createElement("section"); section.className = "section";
      const heading = document.createElement("div"); heading.className = "checks-heading";
      const label = document.createElement("span"); label.className = "section-title"; label.textContent = "Checks";
      const refresh = button("↻", () => { void load(); }, { shortcut: "r", className: "refresh" });
      const passed = document.createElement("span"); passed.className = "passed"; passed.textContent = `${checks.filter((check) => classifyCheck(check) === "pass").length} passed`;
      heading.append(label, refresh, passed);
      const list = document.createElement("div"); list.className = "check-list";
      for (const [index, check] of checks.entries()) {
        const result = classifyCheck(check);
        const row = document.createElement("div"); row.className = "check";
        const number = document.createElement("span"); number.className = "index"; number.textContent = String(index + 1);
        const icon = document.createElement("span"); icon.className = `icon ${result}`; icon.textContent = result === "pass" ? "✓" : result === "fail" ? "✗" : "◌";
        const name = document.createElement("span"); name.className = "name"; name.textContent = check.workflowName || check.name || "Unnamed check";
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
      if (merge) merge.textContent = `Merge with ${strategy}`;
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
      busy = true; render();
      try { await call("github.merge", { strategy: selectedStrategy }); await load(); }
      catch (error) { render(String(error)); } finally { busy = false; }
    }

    function confirmFocusedMergeStrategy() {
      const focused = document.activeElement;
      if (!(focused instanceof HTMLButtonElement) || !focused.matches("button[data-merge-strategy]")) return false;
      const confirm = content.querySelector("button[data-merge-confirm]");
      if (!(confirm instanceof HTMLButtonElement) || confirm.disabled) return false;
      confirm.click();
      return true;
    }

    function renderPr(pr) {
      appendHeader(pr);
      const checks = Array.isArray(pr.checks) ? pr.checks : [];
      const failed = checks.some((check) => classifyCheck(check) === "fail");
      appendChecks(checks);
      if (pr.conflicting && !isMerged(pr)) appendWarning("", "⚠ PR has merge conflicts", "Resolve conflicts on GitHub or rebase locally before merging.");
      if (pr.merge_blocked && !isMerged(pr)) appendWarning("error", "⚠ Merge blocked", "Merge is blocked by repository rules, reviews, or checks.");

      if (isDraft(pr)) {
        const section = document.createElement("section"); section.className = "section";
        section.append(button("Mark as ready", async () => { busy = true; render(); try { await call("github.markReady"); await load(); } catch (error) { render(String(error)); } finally { busy = false; } }, { shortcut: "shift+r", className: "draft" }));
        content.append(section);
      }

      if (!isMerged(pr) && !isDraft(pr)) {
        const section = document.createElement("section"); section.className = "section";
        const label = document.createElement("div"); label.className = "section-title"; label.textContent = "Merge";
        const strategies = document.createElement("div"); strategies.className = "strategies";
        for (const strategy of ["squash", "merge", "rebase"]) {
          const choice = button(strategy, () => selectMergeStrategy(strategy), { disabled: !canMerge(pr) });
          choice.dataset.mergeStrategy = strategy;
          choice.setAttribute("aria-pressed", String(strategy === selectedStrategy));
          strategies.append(choice);
        }
        const merge = button(`Merge with ${selectedStrategy}`, () => { void mergeSelected(pr); }, { disabled: !canMerge(pr), className: "merge" });
        merge.dataset.mergeConfirm = "";
        const help = document.createElement("p"); help.className = "shortcut-help"; help.innerHTML = `<kbd>S</kbd> selects a strategy; <kbd>Enter</kbd> or <kbd>Space</kbd> confirms it.`;
        section.append(label, strategies, merge, help); content.append(section);
      }

      if (failed) {
        const section = document.createElement("section"); section.className = "section";
        section.append(button("Send failures to agent", async () => { busy = true; render(); try { const result = await call("github.failureLogs"); render(result.message || "No failure logs returned"); } catch (error) { render(String(error)); } finally { busy = false; } }, { shortcut: "f", className: "failure" }));
        content.append(section);
      }
      appendFooter([["O", "open"], ["S", "strategy"], ["R", "refresh"]]);
    }

    function render(error = "") {
      content.replaceChildren();
      if (!sessionId) { renderSetup("Select a PlaneAI session before opening this panel."); return; }
      if (!snapshot) { appendHeader(null); appendStatus("Refreshing GitHub status…", error); return; }
      if (!snapshot.applicable) { renderSetup(snapshot.reason || "GitHub is not applicable to this session."); return; }
      if (creating) { renderCreate(); return; }
      if (snapshot.pr) { renderPr(snapshot.pr); if (error) appendStatus("GitHub status could not be loaded.", error); return; }
      appendHeader(null);
      const section = document.createElement("section"); section.className = "section";
      section.innerHTML = `<div class="section-title">No pull request</div><p class="muted">Create a pull request for the selected session branch.</p>`;
      section.append(button("Create pull request", async () => { try { defaults = await call("github.defaults"); creating = true; render(); } catch (loadError) { render(String(loadError)); } }, { shortcut: "c" }));
      content.append(section);
      appendFooter([["C", "create"], ["Esc", "close"]]);
    }

    function triggerShortcut(shortcut) {
      if (shortcut === "s") return cycleMergeStrategy();
      const action = content.querySelector(`[data-shortcut="${shortcut}"]`);
      if (!(action instanceof HTMLButtonElement) || action.disabled) return false;
      action.click();
      return true;
    }

    function handleKeydown(event) {
      if (disposed || event.repeat || event.altKey || event.ctrlKey || event.metaKey || editableTarget(event.target)) return;
      if (event.key === "Enter" || event.key === " ") {
        if (!confirmFocusedMergeStrategy()) return;
        event.preventDefault();
        event.stopPropagation();
        return;
      }
      const shortcut = event.key === "R" ? "shift+r" : event.key.toLowerCase();
      if (!new Set(["r", "c", "o", "shift+r", "s", "f"]).has(shortcut) || !triggerShortcut(shortcut)) return;
      event.preventDefault();
      event.stopPropagation();
    }

    async function load() {
      if (!sessionId) { render(); return; }
      snapshot = null; render();
      try { snapshot = await call("github.status"); render(); }
      catch (error) { snapshot = null; render(String(error)); }
    }

    window.addEventListener("keydown", handleKeydown, true);
    void load();
    return () => { disposed = true; window.removeEventListener("keydown", handleKeydown, true); root.replaceChildren(); };
  },
};

export default githubEntrypoint;
