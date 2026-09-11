type GithubTitlebarContext = {
  session?: { id?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<any>;
    navigation: { open: (pluginId: string, contributionId: string) => void };
  };
};

type GithubTitlebarEntrypoint = {
  mount: (root: HTMLElement, context: GithubTitlebarContext) => () => void;
};

const githubTitlebarEntrypoint: GithubTitlebarEntrypoint = {
  mount(root, context) {
    const sessionId = context.session?.id;
    let disposed = false;
    const button = document.createElement("button");
    button.type = "button";
    button.disabled = true;
    button.setAttribute("aria-label", "Open GitHub pull request");

    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        html, body, main { height:100%; min-height:0; overflow:hidden; }
        main { display:flex; align-items:stretch; }
        button { width:100%; min-height:25px; height:25px; border-radius:7px; padding:2px 9px; font:600 11.5px/18px var(--planeai-font-sans); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; }
        button[data-state="ready"] { color:var(--planeai-success); background:color-mix(in srgb, var(--planeai-success) 14%, var(--planeai-surface-raised)); }
        button[data-state="create"] { color:var(--planeai-text-muted); }
      </style>`;
    page.append(button);
    root.replaceChildren(page);

    const setButton = (label, state, disabled) => {
      if (disposed) return;
      button.textContent = label;
      button.dataset.state = state;
      button.disabled = disabled;
    };
    const openPanel = () => context.host.navigation.open("github", "pull-request");
    button.addEventListener("click", openPanel);

    async function load() {
      if (!sessionId) {
        setButton("GitHub", "unavailable", true);
        return;
      }
      setButton("GitHub…", "loading", true);
      try {
        const status = await context.host.call("github.status", { session_id: sessionId });
        if (!status?.applicable) {
          setButton("GitHub", "unavailable", true);
          return;
        }
        if (!status.pr?.url) {
          setButton("＋ Create PR", "create", false);
          return;
        }
        const number = status.pr.url.match(/\/(?:pull|pulls)\/(\d+)(?:$|[?#])/i)?.[1];
        setButton(number ? `PR #${number}` : "Pull request", "ready", false);
      } catch (_error) {
        setButton("GitHub", "unavailable", true);
      }
    }

    void load();
    return () => {
      disposed = true;
      button.removeEventListener("click", openPanel);
      root.replaceChildren();
    };
  },
};

export default githubTitlebarEntrypoint;
