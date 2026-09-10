const githubEntrypoint = {
  mount(root, context) {
    const sessionId = context.session?.id;
    let snapshot = null;
    let defaults = null;
    let creating = false;
    let busy = false;
    let disposed = false;

    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        main { height:100%; overflow:auto; padding:var(--planeai-space-5); }
        .wrap { max-width:820px; margin:0 auto; display:grid; gap:var(--planeai-space-4); }
        .card { border:1px solid var(--planeai-border); border-radius:var(--planeai-radius); padding:var(--planeai-space-4); background:var(--planeai-surface); display:grid; gap:var(--planeai-space-3); }
        .row { display:flex; gap:var(--planeai-space-2); align-items:center; flex-wrap:wrap; }
        .spread { justify-content:space-between; }
        .muted { color:var(--planeai-text-muted); }
        .error { color:var(--planeai-danger); white-space:pre-wrap; }
        .ok { color:var(--planeai-success); }
        .warn { color:var(--planeai-warning); }
        .checks { display:grid; gap:6px; }
        .check { display:flex; justify-content:space-between; gap:12px; border-top:1px solid var(--planeai-border); padding-top:6px; }
        label { display:grid; gap:6px; font-weight:600; }
        textarea { width:100%; resize:vertical; }
        .hidden { display:none; }
        .pill { border-radius:999px; padding:2px 8px; font:12px var(--planeai-font-mono); background:var(--planeai-surface-raised); }
      </style>
      <div class="wrap">
        <section class="card">
          <div class="row spread"><div><h1>GitHub</h1><p class="muted">Pull request and CI integration</p></div><button data-refresh type="button">Refresh</button></div>
          <p data-status class="muted" role="status">Loading…</p>
          <p data-error class="error" role="alert"></p>
        </section>
        <section data-panel class="card"></section>
      </div>`;
    root.replaceChildren(page);

    const statusEl = page.querySelector("[data-status]");
    const errorEl = page.querySelector("[data-error]");
    const panel = page.querySelector("[data-panel]");
    const setStatus = (value) => { if (!disposed) statusEl.textContent = value; };
    const setError = (value = "") => { if (!disposed) errorEl.textContent = value; };
    const call = (method, params = {}) => context.host.call(method, { session_id: sessionId, ...params });

    function button(label, handler, disabled = false) {
      const element = document.createElement("button");
      element.type = "button";
      element.textContent = label;
      element.disabled = disabled || busy;
      element.addEventListener("click", handler);
      return element;
    }

    function renderSetup(message) {
      panel.replaceChildren();
      const title = document.createElement("h2"); title.textContent = "GitHub is unavailable";
      const detail = document.createElement("p"); detail.className = "muted"; detail.textContent = message;
      const help = document.createElement("p"); help.className = "muted"; help.textContent = "Install GitHub CLI and authenticate with: gh auth login";
      panel.append(title, detail, help);
    }

    function renderCreate() {
      panel.replaceChildren();
      const title = document.createElement("h2"); title.textContent = "Create pull request";
      const form = document.createElement("form"); form.className = "card";
      form.style.padding = "0"; form.style.border = "0"; form.style.background = "transparent";
      const titleField = document.createElement("input"); titleField.value = defaults?.title || ""; titleField.required = true;
      const bodyField = document.createElement("textarea"); bodyField.rows = 10; bodyField.value = defaults?.body || "";
      const baseField = document.createElement("input"); baseField.value = defaults?.base_branch || "main"; baseField.required = true;
      const draft = document.createElement("input"); draft.type = "checkbox";
      const labelled = (label, field) => { const wrapper = document.createElement("label"); wrapper.textContent = label; wrapper.append(field); return wrapper; };
      const draftLabel = document.createElement("label"); draftLabel.className = "row"; draftLabel.append(draft, document.createTextNode("Draft pull request"));
      const actions = document.createElement("div"); actions.className = "row";
      actions.append(button("Cancel", () => { creating = false; render(); }), button("Create", async () => {
        busy = true; render(); setError();
        try { await call("github.create", { title: titleField.value, body: bodyField.value, base_branch: baseField.value, draft: draft.checked }); creating = false; await load(); }
        catch (error) { setError(String(error)); } finally { busy = false; render(); }
      }));
      form.addEventListener("submit", (event) => { event.preventDefault(); actions.querySelectorAll("button")[1]?.click(); });
      form.append(labelled("Title", titleField), labelled("Body", bodyField), labelled("Base branch", baseField), draftLabel, actions);
      panel.append(title, form);
    }

    function renderPr(pr) {
      panel.replaceChildren();
      const heading = document.createElement("div"); heading.className = "row spread";
      const title = document.createElement("h2"); title.textContent = "Pull request";
      const state = document.createElement("span"); state.className = "pill"; state.textContent = pr.state || "open";
      heading.append(title, state);
      panel.append(heading);
      if (pr.url) {
        const url = document.createElement("p"); url.className = "muted"; url.textContent = pr.url;
        const open = button("Open on GitHub", () => context.host.navigation.openExternal(pr.url));
        panel.append(url, open);
      }
      if (pr.conflicting) { const conflict = document.createElement("p"); conflict.className = "warn"; conflict.textContent = "Pull request has merge conflicts."; panel.append(conflict); }
      if (pr.merge_blocked) { const blocked = document.createElement("p"); blocked.className = "warn"; blocked.textContent = "Merge is blocked by repository rules, reviews, or checks."; panel.append(blocked); }
      const checks = Array.isArray(pr.checks) ? pr.checks : [];
      if (checks.length) {
        const section = document.createElement("section"); section.className = "checks";
        const label = document.createElement("h3"); label.textContent = "Checks"; section.append(label);
        for (const check of checks) { const row = document.createElement("div"); row.className = "check"; const name = document.createElement("span"); name.textContent = check.workflowName || check.name || "Unnamed check"; const conclusion = document.createElement("span"); conclusion.className = check.conclusion === "FAILURE" ? "error" : "muted"; conclusion.textContent = check.conclusion || check.status || "pending"; row.append(name, conclusion); section.append(row); }
        panel.append(section);
      }
      const actions = document.createElement("div"); actions.className = "row";
      if (pr.state === "draft") actions.append(button("Mark ready", async () => { busy = true; render(); try { await call("github.markReady"); await load(); } catch (error) { setError(String(error)); } finally { busy = false; render(); } }));
      if (pr.state === "open") {
        for (const strategy of ["squash", "merge", "rebase"]) actions.append(button(`Merge (${strategy})`, async () => { busy = true; render(); try { await call("github.merge", { strategy }); await load(); } catch (error) { setError(String(error)); } finally { busy = false; render(); } }, pr.conflicting || pr.merge_blocked));
      }
      if (checks.some((check) => check.conclusion === "FAILURE")) actions.append(button("Get failure logs", async () => { busy = true; render(); try { const result = await call("github.failureLogs"); setError(result.message || "No failure logs returned"); } catch (error) { setError(String(error)); } finally { busy = false; render(); } }));
      panel.append(actions);
    }

    function render() {
      if (!sessionId) { renderSetup("Select a PlaneAI session before opening this panel."); return; }
      if (!snapshot) { panel.replaceChildren(); return; }
      if (!snapshot.applicable) { renderSetup(snapshot.reason || "This session does not use github.com."); return; }
      if (creating) { renderCreate(); return; }
      if (snapshot.pr) { renderPr(snapshot.pr); return; }
      panel.replaceChildren();
      const title = document.createElement("h2"); title.textContent = "No pull request";
      const detail = document.createElement("p"); detail.className = "muted"; detail.textContent = "Create a pull request for the selected session branch.";
      panel.append(title, detail, button("Create pull request", async () => { setError(); try { defaults = await call("github.defaults"); creating = true; render(); } catch (error) { setError(String(error)); } }));
    }

    async function load() {
      if (!sessionId) { render(); return; }
      setStatus("Refreshing GitHub status…"); setError();
      try { snapshot = await call("github.status"); setStatus(snapshot.applicable ? "GitHub status is current." : "GitHub is not applicable to this session."); }
      catch (error) { snapshot = null; setStatus("GitHub status could not be loaded."); setError(String(error)); }
      render();
    }

    page.querySelector("[data-refresh]").addEventListener("click", () => { void load(); });
    void load();
    return () => { disposed = true; root.replaceChildren(); };
  },
};

export default githubEntrypoint;
