type CheckSummary = { state?: "passing" | "failing" | "pending" };
type GithubIndicatorContext = {
  session?: { id?: string };
  host: {
    call: (method: string, params?: Record<string, unknown>) => Promise<{ check_summary?: CheckSummary | null }>;
    data: { onChanged: (listener: () => void) => () => void };
  };
};

type GithubIndicatorEntrypoint = {
  mount: (root: HTMLElement, context: GithubIndicatorContext) => () => void;
};

const labels = {
  passing: "GitHub CI passing",
  failing: "GitHub CI failing",
  pending: "GitHub CI pending",
} as const;
const githubIndicatorEntrypoint: GithubIndicatorEntrypoint = {
  mount(root, context) {
    let disposed = false;
    root.replaceChildren();
    function reportContentWidth(width: number): void {
      window.parent.postMessage({ type: "content-width", width }, "*");
    }

    async function load() {
      root.replaceChildren();
      reportContentWidth(0);
      const sessionId = context.session?.id;
      if (!sessionId) return;
      try {
        const response = await context.host.call("github.indicator", { session_id: sessionId });
        const state = response.check_summary?.state;
        if (disposed || !state || !(state in labels)) return;
        const icon = document.createElement("span");
        icon.className = `github-check github-check-${state}`;
        icon.setAttribute("role", "img");
        icon.setAttribute("aria-label", labels[state]);
        if (state === "pending") {
          icon.classList.add("github-check-dot");
        } else {
          const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
          svg.setAttribute("viewBox", "0 0 24 24");
          svg.setAttribute("fill", "none");
          svg.setAttribute("stroke", "currentColor");
          svg.setAttribute("stroke-width", "2");
          svg.setAttribute("stroke-linecap", "round");
          svg.setAttribute("stroke-linejoin", "round");
          svg.innerHTML = state === "passing"
            ? '<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>'
            : '<circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/>';
          icon.append(svg);
        }
        const page = document.createElement("main");
        page.innerHTML = `<style>
          html,body,main{margin:0;width:100%;height:100%;overflow:hidden;background:transparent}
          main{display:grid;place-items:center}
          .github-check{display:block;width:12px;height:12px}
          .github-check svg{display:block;width:12px;height:12px}
          .github-check-passing{color:var(--planeai-success)}
          .github-check-failing{color:var(--planeai-danger)}
          .github-check-dot{width:8px;height:8px;border-radius:9999px;background:#f59e0b;animation:pulse-dot 1.6s ease-in-out infinite}
          @keyframes pulse-dot{0%,100%{opacity:1;transform:scale(1)}50%{opacity:.4;transform:scale(.78)}}
          @media (prefers-reduced-motion:reduce){.github-check-dot{animation:none}}
        </style>`;
        page.append(icon);
        root.replaceChildren(page);
        reportContentWidth(16);
      } catch (_) {
        // Cached indicators are best-effort and intentionally remain absent on read failures.
      }
    }
    const unsubscribe = context.host.data.onChanged(() => { void load(); });
    void load();
    return () => { disposed = true; unsubscribe(); root.replaceChildren(); reportContentWidth(0); };
  },
};

export default githubIndicatorEntrypoint;
