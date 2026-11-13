# hc-status: opencode → herdr status reports

A tiny [opencode](https://opencode.ai) v2 plugin that makes opencode report
its status to [herdr-core](https://github.com/herdrdev/herdr-core) (`hc`)
through **OSC 7501, the Program Status Protocol**, whenever opencode runs
inside an hc terminal. This is what lets the host core (and ois) show
`working / idle / done / blocked / error` per agent.

## What it reports

opencode v2.1.4 already sends `working`, `idle` and `blocked` natively
(OpenTUI speaks 7501 when the terminal advertises the capability, which hc
does). The plugin adds what the native reporter does not send:

| hc status  | source   | when |
|------------|----------|------|
| `idle`     | plugin   | once at startup (hc template readiness: "wait for the first status report") |
| `working`  | native   | while a session executes (with the session title) |
| `working`  | plugin   | while a failed model call is being retried (msg: "retrying after error (attempt N)") |
| `blocked`  | native   | a permission dialog is open (`kind=permission`, title, msg) |
| `done`     | plugin   | the last running execution finished successfully |
| `error`    | plugin   | an execution failed and nothing else is running (msg: the failure) |
| `idle`     | plugin   | an execution was interrupted (user / superseded / inactivity) — not `done` |

Details:

- Root record only (`id` empty), `app=opencode`. The native reporter also
  sends per-session records keyed by session id; hc keeps both.
- The plugin's `done`/`idle` reports are written ~300 ms after the native
  `idle` so hc rests on the final word.
- A session error while other sessions keep working is not reported as
  `error` (the terminal is still working); the error message is lost in that
  case. One agent terminal normally has one root session, so this is rare.
- Works with the TUI (`opencode`, `opencode --auto`). It does nothing for
  headless `opencode run` processes (no TUI, no tty, no reports at all).

## Install

Global, for every opencode the user starts (this is how bento boxes get it:
the host's `~/.config/opencode` is mounted read-only into every box):

```sh
cp -r bento/agents/opencode/hc-status ~/.config/opencode/plugins/
```

opencode discovers plugins in `~/.config/opencode/plugins/*/` and loads
`tui.js` as the plugin's TUI part. No dependencies, nothing to build.

Or per project, for testing:

```sh
cp -r bento/agents/opencode/hc-status <project>/.opencode/plugins/
```

## Why a directory with `tui.js` (not a single file)

A plugin that writes to the terminal must run in the TUI process — that is
the only opencode process attached to the hc terminal. Server parts run in
the (detached) server process, which has no tty: `/dev/tty` fails with ENXIO
and stdout is not a terminal. opencode v2 loads a single-file plugin as a
server part only; TUI parts are loaded from a plugin directory's `tui.js`.

## Debugging

Set `HC_STATUS_DEBUG=<file>` in opencode's environment and the plugin logs
its reports there. It is a no-op unless `HC_TERMINAL_ID` is set (which hc
provides), so the user's normal opencode is unaffected.

## Try it

```sh
hc run --bg --name agent --cwd <project> -- opencode --auto
hc get agent --json          # -> status: {"app":"opencode","state":"idle"}
hc send agent "list the files here"
hc get agent --json          # -> working, then done
```
