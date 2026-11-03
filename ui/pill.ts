import { checkTone, stateTone, summarize, type PullRequest } from "./pull-requests.js";

const SVG = "http://www.w3.org/2000/svg";

function icon(paths: string, className: string): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.75");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add(className);
  svg.innerHTML = paths;
  return svg;
}

const LAYERS = '<path d="m8 2 6 3.25L8 8.5 2 5.25 8 2Z"/><path d="m2 8.25 6 3.25 6-3.25"/><path d="m2 11.25 6 3.25 6-3.25"/>';
const CHECK = '<path d="m3.5 8.5 3 3 6-7"/>';
const CROSS = '<path d="m4.5 4.5 7 7"/><path d="m11.5 4.5-7 7"/>';

/** Builds the pill for a task's pull requests, or null when it has none. */
export function renderPill(prs: readonly PullRequest[]): HTMLElement | null {
  const summary = summarize(prs);
  if (!summary) return null;
  const pill = document.createElement("span");
  pill.className = "pill";
  pill.dataset.tone = stateTone(summary.current.state);
  const number = document.createElement("span");
  number.className = "number";
  number.textContent = `#${summary.current.number}`;
  pill.append(number);
  if (summary.stacked) {
    const stack = document.createElement("span");
    stack.className = "count";
    stack.append(icon(LAYERS, "layers"), String(summary.others + 1));
    pill.append(stack);
  } else if (summary.others > 0) {
    const more = document.createElement("span");
    more.className = "count";
    more.textContent = `+${summary.others}`;
    pill.append(more);
  }
  if (summary.checks) {
    const ci = summary.checks === "pending" ? document.createElement("span") : icon(summary.checks === "passing" ? CHECK : CROSS, "ci");
    if (summary.checks === "pending") ci.classList.add("ci", "dot");
    ci.dataset.tone = checkTone(summary.checks);
    pill.append(ci);
  }
  return pill;
}

export const PILL_STYLES = `
  html, body { margin:0; height:16px; overflow:hidden; background:transparent; }
  .pill { --tone:var(--planeai-text-muted); display:inline-flex; align-items:center; gap:4px; width:max-content; height:16px; padding:0 5px; border-radius:999px; background:color-mix(in srgb, var(--tone) 14%, transparent); color:var(--tone); font:500 10.5px/16px var(--planeai-font-mono); white-space:nowrap; font-variant-numeric:tabular-nums; }
  .pill[data-tone="success"] { --tone:var(--planeai-success); }
  .pill[data-tone="info"] { --tone:var(--planeai-info); }
  .pill[data-tone="danger"] { --tone:var(--planeai-danger); }
  .count { display:inline-flex; align-items:center; gap:2px; opacity:.8; }
  .layers { width:10px; height:10px; }
  .ci { flex:none; width:10px; height:10px; }
  .ci.dot { width:6px; height:6px; margin:0 2px; border-radius:999px; background:currentColor; }
  .ci[data-tone="success"] { color:var(--planeai-success); }
  .ci[data-tone="warning"] { color:var(--planeai-warning); }
  .ci[data-tone="danger"] { color:var(--planeai-danger); }
`;
