type GithubSettingsContext = {
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<any>;
    data: { notify: (message: string, kind?: "success" | "error") => void };
  };
};

type GithubSettingsEntrypoint = {
  mount: (root: HTMLElement, context: GithubSettingsContext) => () => void;
};

const statuses = [
  ["", "Do not change task"],
  ["todo", "To do"],
  ["in_progress", "In progress"],
  ["in_review", "In review"],
  ["done", "Done"],
];

const githubSettingsEntrypoint: GithubSettingsEntrypoint = {
  mount(root, context) {
    let disposed = false;
    let settings: any = null;
    let saving = false;
    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        main { padding:var(--planeai-space-5); color:var(--planeai-text); }
        h2 { margin:0 0 var(--planeai-space-2); font-size:15px; }
        p { color:var(--planeai-text-muted); font-size:12px; line-height:1.45; }
        .card { display:grid; gap:var(--planeai-space-3); max-width:580px; padding:var(--planeai-space-4); border:1px solid var(--planeai-border); border-radius:8px; background:var(--planeai-surface-raised); }
        label { display:grid; gap:6px; font-size:12px; font-weight:600; }
        select, button { min-height:32px; }
        button { justify-self:start; padding:0 12px; border-color:var(--planeai-accent); background:var(--planeai-accent); color:var(--planeai-on-accent); }
        .error { color:var(--planeai-danger); white-space:pre-wrap; }
      </style>
      <h2>GitHub pull requests</h2>
      <p>Optionally move a session’s linked PlaneAI task when its pull request is opened or merged. Changes apply after the next persisted PR state transition.</p>
      <div data-content></div>`;
    root.replaceChildren(page);
    const content = page.querySelector("[data-content]")!;

    const select = (label: string, value: string | null) => {
      const field = document.createElement("label");
      field.append(label);
      const input = document.createElement("select");
      for (const [status, name] of statuses) {
        const option = document.createElement("option"); option.value = status; option.textContent = name; option.selected = status === (value || ""); input.append(option);
      }
      field.append(input);
      return input;
    };

    const render = (error = "") => {
      content.replaceChildren();
      if (!settings) { content.textContent = "Loading GitHub settings…"; return; }
      const card = document.createElement("section"); card.className = "card";
      const onOpen = select("When a pull request opens", settings.task_transitions?.on_open);
      const onMerge = select("When a pull request merges", settings.task_transitions?.on_merge);
      const save = document.createElement("button"); save.type = "button"; save.disabled = saving; save.textContent = saving ? "Saving…" : "Save transitions";
      save.addEventListener("click", async () => {
        saving = true; render();
        try {
          settings = await context.host.call("github.settings.update", { task_transitions: { on_open: onOpen.value || null, on_merge: onMerge.value || null } });
          context.host.data.notify("GitHub task transitions saved", "success");
          render();
        } catch (saveError) { saving = false; render(String(saveError)); return; }
        saving = false;
      });
      card.append(onOpen, onMerge, save); content.append(card);
      if (error) { const message = document.createElement("p"); message.className = "error"; message.textContent = error; content.append(message); }
    };

    void context.host.call("github.settings").then((value) => { if (!disposed) { settings = value; render(); } }).catch((error) => { if (!disposed) render(String(error)); });
    return () => { disposed = true; root.replaceChildren(); };
  },
};

export default githubSettingsEntrypoint;
