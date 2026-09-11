type GithubPullRequest = {
  url?: string;
};

type GithubStatus = {
  applicable?: boolean;
  pr?: GithubPullRequest | null;
};

type GithubTitlebarContext = {
  session?: { id?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<GithubStatus>;
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
        button { width:fit-content; max-width:100%; min-height:25px; height:25px; border:1px solid transparent; border-radius:7px; padding:0 9px; font:500 11.5px/18px var(--planeai-font-sans); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; color:var(--planeai-text-muted); background:transparent; cursor:pointer; }
        button[data-state="ready"] { color:var(--planeai-success); background:rgba(63,185,80,.18); }
        button[data-state="create"] { border-color:var(--planeai-border); padding:0 10px; }
        button[data-state="create"]:hover { background:var(--planeai-surface-raised); }
      </style>`;
    page.append(button);
    root.replaceChildren(page);

    const setButton = (label: string, state: string, disabled: boolean) => {
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
        if (!status.applicable) {
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
