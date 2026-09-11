# PlaneAI GitHub plugin

A private, local PlaneAI plugin that owns GitHub pull-request and CI integration. It uses the authenticated [`gh`](https://cli.github.com/) CLI and `git`; it never stores GitHub credentials.

## Scope

The plugin is intentionally GitHub-specific. PlaneAI core supplies generic plugin capabilities, a selected-session panel, and repository context; GitHub-specific PR state, checks, merge behavior, and UI belong here. v0.1 targets `github.com`, matching the previous bundled behavior. GitHub Enterprise is deferred.

## Requirements

- PlaneAI with `sessions.repository-context` and `session.panel` plugin-platform support.
- `git` and `gh` on `PATH`.
- `gh auth login` completed for `github.com`.

## Install

Download and extract the archive for your platform, then select **Preferences → Plugins → Install local package** and choose the extracted directory. Local plugins are trusted native executables; install only packages you trust.

## Panel keyboard shortcuts

`Cmd+Shift+P` on macOS (`Ctrl+Shift+P` elsewhere) opens the GitHub pull-request session panel whenever the plugin is running for the selected session. PlaneAI falls back to its legacy pull-request panel only when this plugin contribution is unavailable.

When the GitHub session panel has focus, use `R` to refresh, `C` to create a pull request, `O` to open the pull request in GitHub, `Shift+R` to mark a draft ready, and `F` to retrieve failed-check logs. `S` cycles the available merge strategies; activate the focused strategy with `Enter` or `Space`. This deliberate two-step merge interaction avoids merging from a bare shortcut.

## Development

The editable browser UI is TypeScript in `ui/entry.ts` and `ui/titlebar.ts`. PlaneAI loads a browser JavaScript Blob, so `make package` compiles those sources into `build/ui/*.js` and stages the emitted JavaScript at the manifest paths `ui/*.js` in the package.

```bash
pnpm install --frozen-lockfile
make test
make package
# then test the staged package with a compatible PlaneAI host
planeai-cli plugin test --package dist/planeai-plugin-github
```

For local development, install **`dist/planeai-plugin-github`** in PlaneAI—not the repository root. The host requires the platform binary at `bin/<platform>/planeai-plugin-github`, which `make package` stages into that `dist` directory.

The sidecar speaks newline-delimited JSON-RPC on stdin/stdout. Stdout is protocol-only; diagnostics go to stderr. The package UI remains one self-contained browser ESM JavaScript file because PlaneAI loads an entrypoint source file rather than an asset graph.

## Durable state

With the manifest-granted `settings` capability, the sidecar persists only its `github` namespace through `host.settings.get` and `host.settings.replace`; it read-merges-replaces so unrelated plugin settings survive. The namespace is a strict version-1 JSON document containing `pull_requests` keyed by session ID (session ID, PR URL/state, known remote/branch, and update timestamp) and a reconciliation ledger. Missing state starts as an empty v1 document; malformed or unsupported documents are rejected rather than migrated implicitly. `github.status`, `github.create`, `github.link`, and `github.merge` update mappings only after their GitHub operation succeeds; a successful status lookup with no PR clears that session mapping.

`github.reconcile` remains an explicit, on-demand RPC. It writes a unique fenced `running` attempt before checking active sessions, records a previously persisted running attempt as recovered, and then writes `idle` counts/timestamps or bounded `failed` error details (including cancellation). It never schedules or enables background reconciliation.

## Release artifacts

The release workflow builds package archives for macOS arm64/x64, Linux x64/arm64, and Windows x64/arm64. Each archive contains `planeai-plugin.json`, generated `ui/entry.js` and `ui/titlebar.js`, and the binary under the manifest-declared `bin/<platform>/` path.
