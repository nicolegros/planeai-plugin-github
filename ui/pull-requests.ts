export type PullRequestState = "draft" | "open" | "merged" | "closed";
export type CheckState = "passing" | "pending" | "failing";
export type PullRequestSource = "branch" | "created" | "manual" | "stack";
export type Tone = "success" | "warning" | "danger" | "info" | "muted";

/** One pull request of a session's task, as `github.summary` and `github.status` return it. */
export type PullRequest = {
  host: string;
  repository: string;
  number: number;
  url: string;
  source: PullRequestSource;
  state: PullRequestState;
  title?: string | null;
  checks?: CheckState | null;
  head_branch?: string | null;
  base_branch?: string | null;
  updated_at?: string | null;
};

/** One check of a pull request, classified by the plugin backend. */
export type CheckItem = {
  name: string | null;
  workflowName: string | null;
  result: "pass" | "fail" | "pending";
};

/** The checks and mergeability `github.status` returns for each of the task's pull requests. */
export type PullRequestDetail = {
  url: string;
  number: number;
  state: PullRequestState;
  checks: CheckItem[];
  conflicting: boolean;
  merge_blocked: boolean;
  merge_methods: string[];
};

export type GithubStatus =
  | { applicable: false; reason?: string }
  | {
      applicable: true;
      remote: string;
      repository: string;
      branch: string;
      pull_requests: PullRequest[];
      /** The session's own pull request, which the panel highlights first. */
      own_url: string | null;
      details: PullRequestDetail[];
      /** Whether this refresh changed the stored pull requests. */
      changed: boolean;
    };

export type PullRequestSummary = {
  current: PullRequest;
  /** Pull requests besides the current one. */
  others: number;
  /** Every pull request belongs to one base-branch chain. */
  stacked: boolean;
  /** The worst check result across open pull requests. */
  checks: CheckState | null;
};

const CHECK_SEVERITY: Record<CheckState, number> = { passing: 0, pending: 1, failing: 2 };

export const isLive = (pr: PullRequest): boolean => pr.state === "open" || pr.state === "draft";

const sameRepository = (a: PullRequest, b: PullRequest): boolean => a.host === b.host && a.repository === b.repository;

const newestFirst = (a: PullRequest, b: PullRequest): number =>
  (b.updated_at ?? "").localeCompare(a.updated_at ?? "") || b.number - a.number;

/** The pull requests ordered from the trunk up when they form one base-branch chain, else null. */
export function stackOrder(prs: readonly PullRequest[]): PullRequest[] | null {
  if (prs.length < 2) return null;
  const parentOf = (pr: PullRequest) =>
    prs.find((other) => other !== pr && sameRepository(other, pr) && other.head_branch && other.head_branch === pr.base_branch);
  const bottoms = prs.filter((pr) => !parentOf(pr));
  if (bottoms.length !== 1) return null;
  const ordered = [bottoms[0]];
  while (ordered.length < prs.length) {
    const top = ordered[ordered.length - 1];
    const children = prs.filter((pr) => parentOf(pr) === top);
    if (children.length !== 1) return null;
    ordered.push(children[0]);
  }
  return ordered;
}

/** The single open pull request, else the top of an open stack, else the most recently updated one. */
export function currentPullRequest(prs: readonly PullRequest[]): PullRequest | null {
  if (!prs.length) return null;
  const live = prs.filter(isLive);
  if (live.length === 1) return live[0];
  const stack = stackOrder(live);
  if (stack) return stack[stack.length - 1];
  return [...(live.length ? live : prs)].sort(newestFirst)[0];
}

/** Failing beats pending beats passing; only open pull requests count. */
export function worstCheck(prs: readonly PullRequest[]): CheckState | null {
  let worst: CheckState | null = null;
  for (const pr of prs) {
    if (!isLive(pr) || !pr.checks || !(pr.checks in CHECK_SEVERITY)) continue;
    if (worst === null || CHECK_SEVERITY[pr.checks] > CHECK_SEVERITY[worst]) worst = pr.checks;
  }
  return worst;
}

export function summarize(prs: readonly PullRequest[]): PullRequestSummary | null {
  const current = currentPullRequest(prs);
  if (!current) return null;
  return { current, others: prs.length - 1, stacked: stackOrder(prs) !== null, checks: worstCheck(prs) };
}

export function stateTone(state: PullRequestState): Tone {
  return state === "open" ? "success" : state === "merged" ? "info" : state === "closed" ? "danger" : "muted";
}

export function checkTone(checks: CheckState | null | undefined): Tone | null {
  return checks === "failing" ? "danger" : checks === "pending" ? "warning" : checks === "passing" ? "success" : null;
}

export const STATE_LABELS: Record<PullRequestState, string> = { draft: "Draft", open: "Open", merged: "Merged", closed: "Closed" };
export const CHECK_LABELS: Record<CheckState, string> = { passing: "CI passing", pending: "CI pending", failing: "CI failing" };
export const SOURCE_LABELS: Record<PullRequestSource, string> = { branch: "Branch", created: "Created", manual: "Linked", stack: "Stack" };

/** Rows in display order: a stack from its top down, otherwise open first, then newest. */
export function displayOrder(prs: readonly PullRequest[]): PullRequest[] {
  const stack = stackOrder(prs);
  if (stack) return [...stack].reverse();
  return [...prs].sort((a, b) => Number(isLive(b)) - Number(isLive(a)) || newestFirst(a, b));
}

/** `Draft`, `Merged`, `Closed`, or the CI result of an open pull request; status leads so truncation keeps it. */
export function statusLabel(pr: PullRequest): string {
  if (pr.state === "draft") return pr.checks ? `Draft, ${CHECK_LABELS[pr.checks]}` : STATE_LABELS.draft;
  if (pr.state === "open") return pr.checks ? CHECK_LABELS[pr.checks] : STATE_LABELS.open;
  return STATE_LABELS[pr.state] ?? pr.state;
}

/** One tooltip line per pull request, toned by failing or pending CI first, then by state. */
export function tooltipLines(prs: readonly PullRequest[]): { text: string; tone: Tone }[] {
  return displayOrder(prs).map((pr) => {
    const ci = isLive(pr) ? checkTone(pr.checks) : null;
    const title = pr.title?.trim() ? ` · ${pr.title.trim()}` : "";
    return { text: `#${pr.number} · ${statusLabel(pr)}${title}`, tone: ci === "danger" || ci === "warning" ? ci : stateTone(pr.state) };
  });
}

/** The repository of a pull request outside the session's, without the owner they share. */
export function foreignRepository(pr: PullRequest, sessionRepository: string | null | undefined): string | null {
  if (!sessionRepository || pr.repository.toLowerCase() === sessionRepository.toLowerCase()) return null;
  const [owner, name] = pr.repository.split("/");
  return owner.toLowerCase() === sessionRepository.split("/")[0].toLowerCase() ? name : pr.repository;
}

/** The panel's highlighted pull request: the kept highlight, else the session's own, else the current one. */
export function resolveSelection(prs: readonly PullRequest[], selectedUrl: string | null, ownUrl: string | null): PullRequest | null {
  return prs.find((pr) => pr.url === selectedUrl) ?? prs.find((pr) => pr.url === ownUrl) ?? currentPullRequest(prs);
}

/** The row one step up or down from the highlight, in display order, clamped at the ends. */
export function moveSelection(prs: readonly PullRequest[], selectedUrl: string | null, delta: 1 | -1): PullRequest | null {
  const order = displayOrder(prs);
  if (!order.length) return null;
  const index = Math.max(0, order.findIndex((pr) => pr.url === selectedUrl));
  return order[Math.min(order.length - 1, Math.max(0, index + delta))];
}

/**
 * Whether the session's branch already has an open pull request, so Create PR has nothing to do.
 * Branch detection keeps a created pull request's source, so one created from this branch counts too.
 */
export function branchHasPullRequest(prs: readonly PullRequest[], branch: string | null | undefined): boolean {
  return prs.some((pr) => isLive(pr) && (pr.source === "branch" || (pr.source === "created" && !!branch && pr.head_branch === branch)));
}

/** The panel footer's shortcut hints, in display order. */
export function panelHints({ prs, selected, mergeable, branch }: { prs: readonly PullRequest[]; selected: PullRequest | null; mergeable: boolean; branch: string | null | undefined }): [string, string][] {
  const hints: [string, string][] = [];
  if (prs.length > 1) hints.push(["↑↓", "select"]);
  if (selected) hints.push(["O", "open"]);
  if (selected && selected.source !== "branch") hints.push(["U", selected.source === "stack" ? "dismiss" : "unlink"]);
  if (mergeable) hints.push(["S", "strategy"]);
  hints.push(["R", "refresh"]);
  if (!branchHasPullRequest(prs, branch)) hints.push(["C", "create"]);
  hints.push(["L", "link"]);
  return hints;
}
