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

## Development

```bash
cargo test
make package
# then test the staged package with a compatible PlaneAI host
planeai-cli plugin test --package dist/planeai-plugin-github
```

The sidecar speaks newline-delimited JSON-RPC on stdin/stdout. Stdout is protocol-only; diagnostics go to stderr. The UI is one self-contained browser ESM module because PlaneAI loads an entrypoint source file rather than an asset graph.

## Release artifacts

The release workflow builds package archives for macOS arm64/x64, Linux x64/arm64, and Windows x64/arm64. Each archive contains `planeai-plugin.json`, `ui/entry.js`, and the binary under the manifest-declared `bin/<platform>/` path.
