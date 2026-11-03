import { PILL_STYLES, renderPill } from "./pill.js";
import { tooltipLines, type PullRequest } from "./pull-requests.js";

/** `github.summary` with `check_repository`: the cached pull requests and whether origin is on github.com. */
type GithubSummary = {
  applicable?: boolean;
  pull_requests?: PullRequest[];
};

type GithubTitlebarContext = {
  session?: { id?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<GithubSummary>;
    navigation: { open: (pluginId: string, contributionId: string) => void };
    data?: { onChanged?: (listener: () => void) => () => void };
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

    const page = document.createElement("main");
    page.innerHTML = `
      <style>
        ${PILL_STYLES}
        html, body, main { height:100%; min-height:0; overflow:hidden; }
        main { display:flex; align-items:center; }
        button { display:inline-flex; align-items:center; width:fit-content; max-width:100%; min-height:25px; height:25px; border:1px solid transparent; border-radius:7px; padding:0 9px; font:500 11.5px/18px var(--planeai-font-sans); white-space:nowrap; overflow:hidden; text-overflow:ellipsis; color:var(--planeai-text-muted); background:transparent; cursor:pointer; }
        button[data-state="pull-requests"] { padding:0; border:0; background:transparent; }
        button[data-state="pull-requests"]:hover:not(:disabled) { background:transparent; filter:brightness(1.08); }
        button[data-state="pull-requests"] .pill { height:25px; gap:5px; padding:0 9px; border-radius:7px; font-size:11.5px; line-height:25px; }
        button[data-state="pull-requests"] .layers, button[data-state="pull-requests"] svg.ci { width:12px; height:12px; }
        button[data-state="create"] { border-color:var(--planeai-border); padding:0 10px; }
        button[data-state="create"]:hover { background:var(--planeai-surface-raised); }
      </style>`;
    page.append(button);
    root.replaceChildren(page);

    const setButton = (content: string | HTMLElement, state: string, disabled: boolean, label: string) => {
      if (disposed) return;
      button.replaceChildren(content);
      button.dataset.state = state;
      button.disabled = disabled;
      button.setAttribute("aria-label", label);
      button.title = label;
    };
    const openPanel = () => context.host.navigation.open("github", "pull-request");
    button.addEventListener("click", openPanel);

    let generation = 0;
    async function load() {
      const current = ++generation;
      if (!sessionId) {
        setButton("GitHub", "unavailable", true, "GitHub");
        return;
      }
      if (current === 1) setButton("GitHub…", "loading", true, "Loading GitHub pull requests");
      try {
        const summary = await context.host.call("github.summary", { session_id: sessionId, check_repository: true });
        if (current !== generation) return;
        if (!summary.applicable) {
          setButton("GitHub", "unavailable", true, "GitHub is not available for this session");
          return;
        }
        const prs = summary.pull_requests ?? [];
        const pill = renderPill(prs);
        if (!pill) {
          setButton("＋ Create PR", "create", false, "Create a pull request");
          return;
        }
        setButton(pill, "pull-requests", false, ["Open pull requests", ...tooltipLines(prs).map((line) => line.text)].join("\n"));
      } catch (_error) {
        if (current === generation) setButton("GitHub", "unavailable", true, "GitHub status could not be loaded");
      }
    }

    const unsubscribe = context.host.data?.onChanged?.(() => { void load(); }) ?? (() => {});
    void load();
    return () => {
      disposed = true;
      unsubscribe();
      button.removeEventListener("click", openPanel);
      root.replaceChildren();
    };
  },
};

export default githubTitlebarEntrypoint;
