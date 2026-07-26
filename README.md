# lx-term

A privacy-focused fork of the [Warp](https://www.warp.dev) terminal, with the
telemetry, crash reporting, auto-update, and account-login paths removed, and a
handful of local quality-of-life features added on top.

Everything that makes Warp a good terminal is still here — the block-based
terminal, the built-in code editor with LSP support, panes and tabs, themes,
workflows, and detection of the CLI agents you run yourself (Claude Code, Codex,
Gemini CLI, and others). What is gone is everything that phoned home.

> [!NOTE]
> This is an independent fork, not affiliated with or supported by Warp. Report
> issues here, not to the Warp team. Upstream lives at
> [warpdotdev/warp](https://github.com/warpdotdev/warp).

## What's different from upstream

### Nothing phones home

- **Telemetry is hard-disabled.** `warpui::telemetry::telemetry_collection_enabled()`
  returns `false`, and every send, batch, queue-to-disk, and flush path in
  `app/src/server/telemetry/` checks it before doing anything. Queued events are
  dropped rather than written out.
- **Crash reporting is hard-disabled.** Sentry is never initialized, regardless
  of the `CrashReporting` feature flag.
- **Auto-update polling is disabled.** The client never asks the update server
  what version it should be running. Update by rebuilding.

### No account, no cloud

Credentials are never loaded — not from secure storage, not from a baked-in
`WARP_USER_SECRET`, and not from `--api-key` (which is accepted and ignored so
scripts don't break). The client runs permanently logged out, so cloud features
that require a Warp account — Warp Drive sync, cloud agents, the hosted AI
agent — are unavailable. Local CLI agents you launch yourself are unaffected.

### Renamed

The OSS binary, process name, and macOS bundle are `lx-term`; logs go to
`lx-term.log`. UI strings that named the product have been rebranded, and the
bundled `oz-platform` skill and Oz agent defaults are removed.

### Added: edit files in the built-in editor from any tool

`lx-term edit` is a blocking editor shim, so tools that shell out to `$EDITOR`
open the file in lx-term's own code editor instead of a terminal editor:

```bash
export KUBE_EDITOR="/path/to/lx-term edit"
kubectl edit cm/some-config
```

The file opens in a split pane next to the still-blocked terminal. Save it and
close the editor tab, and the waiting tool reads the file back and applies it.
Works the same for `EDITOR`, `VISUAL`, and `GIT_EDITOR`.

Outside a local lx-term session — over SSH, or in another terminal — it runs
`$WARP_EDIT_FALLBACK_EDITOR` instead (defaulting to `vi`), so it is safe to
export unconditionally. Pass `--no-wait` to open a file without blocking.

### Added: title bar position

`appearance.tabs.title_bar_position` moves the title bar to the `bottom` of the
window instead of the `top`. Configurable from Settings → Appearance.

## Building

lx-term builds on macOS, Linux, and Windows.

```bash
./script/bootstrap   # platform-specific setup — installs build dependencies
./script/run         # build and run
./script/presubmit   # fmt, clippy, and tests
```

On Linux and Windows `./script/run` invokes `cargo run --bin lx-term`; on macOS
it builds and launches a real `.app` bundle.

`./script/bootstrap` installs the native build dependencies, including `protoc`
(needed by the remote-server crate) and the platform's development headers. If
you skip it, expect build-script failures rather than compile errors.

`./script/test_edit_e2e.py target/debug/lx-term` exercises the `lx-term edit`
shim end to end on a real pty, standing in for the client, so the blocking
contract can be checked without a GUI.

See [WARP.md](WARP.md) for the full engineering guide — coding style, testing,
and platform-specific notes.

## Licensing

The `warpui_core` and `warpui` crates are licensed under the
[MIT license](LICENSE-MIT). The rest of the code in this repository is licensed
under the [AGPL v3](LICENSE-AGPL). This fork does not change either license.

## Upstream

This fork tracks [warpdotdev/warp](https://github.com/warpdotdev/warp).
Contributions that aren't specific to the changes above are usually better sent
upstream — see their [CONTRIBUTING.md](CONTRIBUTING.md). Warp's own
[docs](https://docs.warp.dev/) remain the reference for features this fork
inherits unchanged.

## Code of Conduct

We ask everyone to be respectful and empathetic; this repository follows the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Open Source Dependencies

A few of the [open source dependencies](https://docs.warp.dev/help/licenses)
this project is built on:

- [Tokio](https://github.com/tokio-rs/tokio)
- [NuShell](https://github.com/nushell/nushell)
- [Fig Completion Specs](https://github.com/withfig/autocomplete)
- [Warp Server Framework](https://github.com/seanmonstar/warp)
- [Alacritty](https://github.com/alacritty/alacritty)
- [Hyper HTTP library](https://github.com/hyperium/hyper)
- [FontKit](https://github.com/servo/font-kit)
- [Core-foundation](https://github.com/servo/core-foundation-rs)
- [Smol](https://github.com/smol-rs/smol)
