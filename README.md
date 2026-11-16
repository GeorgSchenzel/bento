# bento

Local [microsandbox](https://microsandbox.dev) VMs for coding agents: each
**box** has its own data disk (the workspace, HOME, docker), its own
loopback address for published ports, channels to host unix sockets, git
credentials, and the shared nix store. No host dir is ever mounted
read-write: files go in and out with `bento cp` or `bento exec`. bento knows
nothing of repos or agents; agents are tools and config files (see the
home-manager presets).

The interface (commands, flags, exit codes, `--json`, the config and
`bento.yaml`) is `contracts.md` v2 in the stew notes; this README is a
summary.

Linux (x86_64, aarch64) with KVM. macOS is for later.

## Commands

```
bento create <name> [-e K=V]… [--json]   make a box and boot it once
bento start <name> [--json]              boot a stopped box
bento restart <name> [--json]            stop + start: applies pending ports and a changed config
bento stop <name>                        processes get SIGHUP, then SIGKILL after 5 s
bento rm <name> [--force]                the box, its data disk and its state
bento exec <name> [--no-start] [-i] [-t] [-w dir] [-e K=V]… -- <cmd> [args…]
bento enter <name> [--no-start] [-w dir] bash in the box
bento cp <name>:<path> <host path>       docker cp's rules, either direction
bento ls [--json]
bento ports <name> [--json]              declared ports, published/busy/pending
bento claude|opencode <name> [args…]     exec -i -t -- <agent> …
bento install | builder [restart|rm]     operator commands
```

Exit codes: 120 usage, 121 no such box, 122 not running with `--no-start`,
123 name exists, 125 any other bento failure; `exec` passes the command's
code through (126/127 not executable/found, 128+n killed by signal n).

## How it works

- One **builder** VM per instance owns the store and runs `nix-daemon`,
  published on host `127.0.0.1:<instance.nix_port>`; boxes mount the store
  read-only and reach the daemon as `host.microsandbox.internal`. Tools from
  the config (`tools:`, pinned flake refs) are built by the builder through
  `exec`, with their own GC roots; the embedded tools (`tools/flake.nix`) hold
  what bento itself needs in a box.
- A box's **data disk** is a sparse ext4 image in `$BENTO_HOME/boxes/<name>/`,
  mounted at `/bento/data`; every boot binds its `workspace`, `home` and
  `docker` dirs to `/workspace`, HOME (the host's HOME path) and
  `/var/lib/docker`. It survives stop/start and recreates; `rm` deletes it.
  The rootfs is reset when a box is recreated (its global config changed).
- **Ports**: `/workspace/bento.yaml` and `/workspace/<repo>/bento.yaml`
  (`ports: [5173]`), published at `<box ip>:<port>` when the box boots. msb
  publishes ports only at a boot, so a port declared later is `pending` until
  `bento restart`. A port busy on the host is skipped with a warning.
- **Channels**: a vsock route per global channel, offered in the box as
  `/run/bento/<name>.sock` by a socat shim started at each boot.
- **exec** runs every command through `bento-run` (`tools/bento-run.c`), so a
  dropped connection gives the command SIGHUP, then SIGKILL after 5 s.
- **Instances**: `BENTO_HOME` (default `~/.local/share/bento`) holds the
  store, the boxes' state and microsandbox's own home (`MSB_HOME`, at most 56
  bytes), so a dev instance never sees another's boxes. With `BENTO_HOME` set,
  the config is `$BENTO_HOME/config.yaml` and must set `instance` (prefix,
  loopback range, nix_port).

## Config

`~/.config/bento/config.yaml`, written by home-manager:

```nix
imports = [ inputs.bento.homeManagerModules.bento ];
programs.bento = {
  enable = true;
  settings = {
    channels.wok.socket = "~/.local/share/wok/wok.sock";
  };
  # token files by path only (e.g. sops-nix's); a missing one is a warning
  git."github.com" = "~/.config/sops-nix/secrets/git/github-com";
  agents.claude = {
    enable = true;                # tool, managed settings, a token per account
    accounts.main = "~/.config/sops-nix/secrets/bento/claude-token";
    model = "opus";               # a token alone doesn't tell claude the plan
  };
  agents.opencode = {
    enable = true;                # tool, generated opencode.json, keys from files
    apiKeyFiles.zai-coding-plan = "~/.config/sops-nix/secrets/zai-api-key";
    settings.model = "zai-coding-plan/glm-5.3";
  };
};
```

Nothing of the host's own agent config is mounted or copied: boxes are made from
this config alone. Every exec gets each claude account's token as
`CLAUDE_ACCOUNT_<NAME>_TOKEN` (the default one as `CLAUDE_CODE_OAUTH_TOKEN`) and each
opencode key as `OPENCODE_KEY_<ID>`, read from their files on the host at that
exec.

Unknown keys fail commands that may boot (125) and are warnings elsewhere.

## Development

```sh
just test   # unit tests, no VM
just e2e    # VM tests under a scratch BENTO_HOME (/tmp/bento-e2e); host only
```

The SDK is used without its `download-binaries` feature: a build never
installs a runtime into `~/.microsandbox`. The runtime comes from the nix
package (`MSB_PATH`, `MSB_LIBKRUNFW_PATH`).
