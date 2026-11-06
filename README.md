# PlaneAI GitHub plugin

A private, local PlaneAI plugin that owns GitHub pull-request and CI integration. It uses the authenticated [`gh`](https://cli.github.com/) CLI and `git`; it never stores GitHub credentials.

## Scope

The plugin is intentionally GitHub-specific. PlaneAI core supplies generic plugin capabilities, a selected-session panel, and repository context; GitHub-specific PR state, checks, merge behavior, and UI belong here. v0.1 targets `github.com`, matching the previous bundled behavior. GitHub Enterprise is deferred.

## Requirements

- PlaneAI with `planeai.plugin-host.v8` (MCP tools and a background service `timeout_ms`), `sessions.read`, `sessions.repository-context`, and `session.panel` plugin-platform support.
- A PlaneAI build that sends `data.onChanged` to an open `session.panel` instead of remounting it, so the panel refreshes in place.
- `git` and `gh` on `PATH`.
- `gh auth login` completed for `github.com`.

## Install

Download and extract the archive for your platform, then select **Preferences → Plugins → Install local package** and choose the extracted directory. Local plugins are trusted native executables; install only packages you trust.

## Pull requests per task

A task can have several pull requests, in one repository or several.
The plugin tracks four kinds of links for each session:

- **Branch**: the pull request the session's branch resolves to, detected on every refresh.
- **Created**: a pull request opened from the panel with **Create PR**.
- **Linked**: a pull request linked by URL with **Link pull request**; linking appends to the list.
- **Stack**: an open pull request discovered through a base-branch chain from a tracked pull request in the same repository.

Stack discovery follows base and head branches as one linear chain: from each of the task's own open pull requests, it walks up to the pull request whose head is its base, and down to the pull request based on its head.
A discovered parent only keeps walking up and a discovered child only keeps walking down, so a parent's other children never join.
When the bottom of a stack merges and GitHub retargets the pull request above it onto the default branch, that pull request keeps walking down toward its children, so a pull request opened on top still joins; a still-open former parent that the task's pull request moved off does not.
The chain ends at the repository's default branch, so a long-lived `main -> production` pull request never joins, and at a branch that several open pull requests share, since that is no chain.
Discovery is bounded to eight stack links per task, and dismissed stack links count toward that bound; unlinked created or manual links do not.
Sessions linked to the same task in the same project share their pull requests, so every session of a task shows the union of their links.
**Unlink** and **Dismiss** remove a link from every session of the task and leave a tombstone, so stack discovery never adds it back; linking it again revives it.
Branch links follow the branch and cannot be unlinked.
When the session's branch resolves to another pull request or to none, the previous branch pull request stays with the task as a **Linked** pull request with its last known state, so it can be unlinked.

## Agent tool

Agents in PlaneAI sessions get the `github_link_pull_request` MCP tool (`mcp__planeai__github_link_pull_request` in Claude Code).
It takes a pull request's github.com `url` and links it to the calling session's task, exactly as **Link pull request** in the panel does, so it shows in the pill, the titlebar, and the panel.
PlaneAI releases after 1.116 refresh those views right after the call; 1.115 and 1.116 show the link at their next refresh.
Agents use it for a pull request the session's branch does not resolve to, such as one in another repository or another branch of a stack.
It declares itself a non-destructive, idempotent write, so PlaneAI's Claude Code and Codex templates run it without asking.
A refused link, such as a URL that is not a github.com pull request, comes back to the agent as an error result.

## Sidebar pill and titlebar

On the Cards sidebar layout, each session row shows a pill after its branch, such as `#656 ✓`.
The pill names the current pull request: the single open one, else the top of an open stack, else the most recently updated one.
It adds `+N` for the other pull requests, or a layers glyph with the count when they all form one stack.
Its mark shows the worst check result across open pull requests (failing, then pending, then passing), and its color follows the current pull request's state.
Hovering it lists every pull request, and clicking it opens the pull-request panel.
A row with no pull request shows no pill.
The titlebar shows the same summary, or **Create PR** when the session has none; like the pill, it reads only the cached pull requests and refreshes when the plugin's data changes.

## Panel keyboard shortcuts

`Cmd+Shift+P` on macOS (`Ctrl+Shift+P` elsewhere) opens the GitHub pull-request session panel whenever the plugin is running for the selected session. PlaneAI falls back to its legacy pull-request panel only when this plugin contribution is unavailable.

The panel lists every pull request of the task, each with its state, CI, title, repository when it differs from the session's, and source, and with **Unlink** or **Dismiss** and an open-on-GitHub action.
Below the list, it details the highlighted pull request: its checks, merge, ready-for-review, and CI failure hand-off.

One row is highlighted at a time, starting with the session's own pull request.
When the plugin's data changes, the open panel refreshes in place, so the highlight, the chosen merge strategy, an open Create PR or Link form with its values, and keyboard focus all stay; reopening the panel starts fresh.
When the GitHub session panel has focus, use these shortcuts:

- `↑`/`↓` or `j`/`k` move the highlight while the list, or none of the panel's controls, has focus.
- `O` opens the highlighted pull request on GitHub.
- `U` unlinks the highlighted pull request, or dismisses it when stack discovery added it.
- `Shift+R` marks the highlighted draft ready for review.
- `F` retrieves the highlighted pull request's failed-check logs and sends them to the currently focused agent session.
- `S` cycles the highlighted pull request's available merge strategies; `Enter` or `Space` merges with the focused one.
  This deliberate two-step merge interaction avoids merging from a bare shortcut.
- `R` refreshes, `C` creates a pull request for the session branch, and `L` links an existing pull request.

## Task transitions

The **Preferences → GitHub** page can automatically transition a session's linked PlaneAI task when its pull requests open or merge.
Each action is optional and can move the task to `todo`, `in_progress`, `in_review`, or `done`.
The opened action runs when the task gains an open, non-draft pull request while it had none.
The merged action runs once nothing of the task is left open, either because a tracked pull request just merged or because its last open pull request was closed, unlinked, or dismissed after a merge that happened after every open pull request was created.
A merge whose time is unknown never counts as later, and a pull request whose creation time is unknown counts as created before any merge.
Linking an already merged pull request alone never counts as a merge.
Creating, linking, unlinking, and merging fail before changing anything when PlaneAI cannot list the task's sessions, and a refresh in that state is shown but not stored, so no transition is ever lost.
Dismissed pull requests do not count.
The plugin invokes the host transition only after it has persisted the change, so repeated reconciliation of an unchanged state never repeats the task action.

## Development

The editable browser UI is TypeScript in `ui/entry.ts`, `ui/titlebar.ts`, `ui/settings.ts`, and `ui/meta.ts`, sharing `ui/pull-requests.ts` and `ui/pill.ts`.
PlaneAI loads each entrypoint as one self-contained browser ESM file, so `make build-ui` type-checks with `tsc` and bundles each entrypoint with esbuild into `build/package/ui/*.js`, which `make package` stages at the manifest paths `ui/*.js`.
`make test` also mounts the panel and titlebar in jsdom (`tests/ui-mount.test.ts`) to check their behavior.

```bash
pnpm install --frozen-lockfile
make test
make package
# then test the staged package with a compatible PlaneAI host
planeai-cli plugin test --package dist/planeai-plugin-github
```

For local development, install **`dist/planeai-plugin-github`** in PlaneAI, not the repository root. The host requires the platform binary at `bin/<platform>/planeai-plugin-github`, which `make package` stages into that `dist` directory.

The sidecar speaks newline-delimited JSON-RPC on stdin/stdout. Stdout is protocol-only; diagnostics go to stderr.

## Durable state

With the manifest-granted `settings` capability, the sidecar persists only its `github` namespace through `host.settings.get` and `host.settings.patch`; host deep-merge semantics preserve unrelated plugin settings.
The namespace is a strict version-4 JSON document containing `pull_requests` keyed by session ID, a reconciliation ledger, and optional `task_transitions`.
Each session holds a list of links keyed by `host`, `repository` (`owner/name`), and `number`, each with its `url`, `source` (`branch`, `created`, `manual`, `stack`, or a tombstone: `stack-dismissed` for a dismissed stack link, `unlinked` for an unlinked created or manual one), and, except for tombstones, the cached `state`, `title`, `checks`, `head_branch`, `base_branch`, GitHub `updated_at`, `created_at`, and `merged_at` (absent in links stored before they existed), and `refreshed_at`.
Older documents migrate in place: version 1 and 2 gain the transition defaults, and version 3 turns each session's single mapping into a one-element list of `manual` links.
A legacy mapping whose URL is not a `github.com` pull request becomes an empty list, with a log line on stderr.
Because the host deletes keys patched to `null`, absent optional fields read as `null`.
Malformed or unsupported documents are rejected rather than migrated implicitly.
`github.status`, `github.create`, `github.link`, `github.unlink`, and `github.merge` update links only after their GitHub operation succeeds; after a merge, the stored state is what GitHub reports next, so a pull request a merge queue holds stays open, and `github.summary` reads the cached links without asking GitHub; with `check_repository`, it also reports whether the session's `origin` is on github.com.

The manifest declares `github.reconcile` as the host-managed background service. PlaneAI runs it on the configured `github_reconciliation_interval_ms` setting (two minutes by default), while it remains safe to call explicitly.
It writes a unique fenced `running` attempt, refreshes every task with an active session through one batched `gh api graphql` query per discovery round (at most two), and then writes `idle` counts and timestamps or bounded `failed` error details, including cancellation.
Its result says whether it `changed` any stored link, so the host refreshes plugin views only after a reconcile that changed something.

## Release artifacts

The automated release workflow builds package archives for macOS arm64, macOS x64 (Intel), Linux x64, and Windows x64. It derives release versions from conventional commits on `main`, creates a draft release, builds the tag on every supported platform, verifies each staged binary handshake against its manifest, and publishes only after every build succeeds. Each archive contains `planeai-plugin.json`, the bundled UI entrypoints including `ui/meta.js`, and the binary under the manifest-declared `bin/<platform>/` path.
