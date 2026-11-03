import { PILL_STYLES, renderPill } from "./pill.js";
import { tooltipLines, type PullRequest } from "./pull-requests.js";

type GithubMetaContext = {
  session?: { id?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<{ pull_requests?: PullRequest[] }>;
    data: { onChanged: (listener: () => void) => () => void };
  };
};

type GithubMetaEntrypoint = {
  mount: (root: HTMLElement, context: GithubMetaContext) => () => void;
};

const githubMetaEntrypoint: GithubMetaEntrypoint = {
  mount(root, context) {
    let disposed = false;
    let generation = 0;
    const style = document.createElement("style");
    style.textContent = PILL_STYLES;
    const post = (message: Record<string, unknown>) => window.parent.postMessage(message, "*");
    const clear = () => {
      root.replaceChildren(style);
      post({ type: "content-width", width: 0 });
      post({ type: "tooltip", lines: [] });
    };

    async function load() {
      const sessionId = context.session?.id;
      const current = ++generation;
      if (!sessionId) return clear();
      try {
        const { pull_requests: prs = [] } = await context.host.call("github.summary", { session_id: sessionId });
        if (disposed || current !== generation) return;
        const pill = renderPill(prs);
        if (!pill) return clear();
        root.replaceChildren(style, pill);
        post({ type: "tooltip", lines: tooltipLines(prs) });
        requestAnimationFrame(() => {
          if (!disposed && current === generation) post({ type: "content-width", width: Math.ceil(pill.getBoundingClientRect().width) });
        });
      } catch (_) {
        // The pill is best-effort and stays hidden when the cached state cannot be read.
        if (!disposed && current === generation) clear();
      }
    }

    clear();
    const unsubscribe = context.host.data.onChanged(() => { void load(); });
    void load();
    return () => { disposed = true; unsubscribe(); clear(); root.replaceChildren(); };
  },
};

export default githubMetaEntrypoint;
