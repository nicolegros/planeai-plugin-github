# PlaneAI GitHub plugin

A private, local PlaneAI plugin that owns GitHub pull-request and CI integration. It uses the authenticated [`gh`](https://cli.github.com/) CLI and `git`; it never stores GitHub credentials.

## Scope

The plugin is intentionally GitHub-specific. PlaneAI core supplies generic plugin capabilities, a selected-session panel, and repository context; GitHub-specific PR state, checks, merge behavior, and UI belong here. v0.1 targets `github.com`, matching the previous bundled behavior. GitHub Enterprise is deferred.

## Requirements

- PlaneAI with `planeai.plugin-host.v5` (the `session.meta` placement), `sessions.read`, `sessions.repository-context`, and `session.panel` plugin-platform support.
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
The chain ends at the repository's default branch, so a long-lived `main -> production` pull request never joins, and at a branch that several open pull requests share, since that is no chain.
Discovery is bounded to eight stack links per task, and dismissed ones count toward that bound.
Sessions linked to the same task in the same project share their pull requests, so every session of a task shows the union of their links.
**Unlink** and **Dismiss** remove a link from every session of the task and leave a tombstone, so stack discovery never adds it back; linking it again revives it.
Branch links follow the branch and cannot be unlinked.

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
Below the list, it details the session's own pull request: its checks, merge, ready-for-review, and CI failure hand-off.

When the GitHub session panel has focus, use `R` to refresh, `C` to create a pull request, `L` to link an existing pull request, `O` to open the current pull request in GitHub, `Shift+R` to mark the session's draft ready, and `F` to retrieve failed-check logs and send them to the currently focused agent session. `S` cycles the available merge strategies; activate the focused strategy with `Enter` or `Space`. This deliberate two-step merge interaction avoids merging from a bare shortcut.

## Task transitions

The **Preferences → GitHub** page can automatically transition a session's linked PlaneAI task when its pull requests open or merge.
Each action is optional and can move the task to `todo`, `in_progress`, `in_review`, or `done`.
The opened action runs when the task gains an open, non-draft pull request while it had none.
The merged action runs when a tracked pull request merges and every remaining tracked pull request of the task is merged or closed.
Removing links, such as unlinking a pull request or a branch moving to another one, never counts as a merge.
Dismissed pull requests do not count.
The plugin invokes the host transition only after it has persisted the change, so repeated reconciliation of an unchanged state never repeats the task action.

## Development

The editable browser UI is TypeScript in `ui/entry.ts`, `ui/titlebar.ts`, `ui/settings.ts`, and `ui/meta.ts`, sharing `ui/pull-requests.ts` and `ui/pill.ts`.
PlaneAI loads each entrypoint as one self-contained browser ESM file, so `make build-ui` type-checks with `tsc` and bundles each entrypoint with esbuild into `build/package/ui/*.js`, which `make package` stages at the manifest paths `ui/*.js`.

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
Each session holds a list of links keyed by `host`, `repository` (`owner/name`), and `number`, each with its `url`, `source` (`branch`, `created`, `manual`, `stack`, or the `stack-dismissed` tombstone), and, except for tombstones, the cached `state`, `title`, `checks`, `head_branch`, `base_branch`, GitHub `updated_at`, and `refreshed_at`.
Older documents migrate in place: version 1 and 2 gain the transition defaults, and version 3 turns each session's single mapping into a one-element list of `manual` links.
A legacy mapping whose URL is not a `github.com` pull request becomes an empty list, with a log line on stderr.
Because the host deletes keys patched to `null`, absent optional fields read as `null`.
Malformed or unsupported documents are rejected rather than migrated implicitly.
`github.status`, `github.create`, `github.link`, `github.unlink`, and `github.merge` update links only after their GitHub operation succeeds, and `github.summary` reads the cached links without asking GitHub; with `check_repository`, it also reports whether the session's `origin` is on github.com.

The manifest declares `github.reconcile` as the host-managed background service. PlaneAI runs it on the configured `github_reconciliation_interval_ms` setting (two minutes by default), while it remains safe to call explicitly.
It writes a unique fenced `running` attempt, refreshes every task with an active session through one batched `gh api graphql` query per discovery round (at most two), and then writes `idle` counts and timestamps or bounded `failed` error details, including cancellation.

## Release artifacts

The automated release workflow builds package archives for macOS arm64, macOS x64 (Intel), Linux x64, and Windows x64. It derives release versions from conventional commits on `main`, creates a draft release, builds the tag on every supported platform, verifies each staged binary handshake against its manifest, and publishes only after every build succeeds. Each archive contains `planeai-plugin.json`, the bundled UI entrypoints including `ui/meta.js`, and the binary under the manifest-declared `bin/<platform>/` path.
