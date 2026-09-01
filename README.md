# bento

Run any repo's devenv inside a [microsandbox](https://microsandbox.dev) box.

## Install (Nix)

The flake exposes `packages`, `overlays.default`, `nixosModules.bento` and
`homeManagerModules.bento` for `x86_64-linux` and `aarch64-linux`.

### Home Manager (recommended)

```nix
# your flake
inputs.bento.url = "github:YOUR_NAME/bento";
```

```nix
# your Home Manager configuration
imports = [ inputs.bento.homeManagerModules.bento ];

programs.bento.enable = true;
```

### NixOS

```nix
inputs.bento.url = "github:YOUR_NAME/bento";
```

```nix
# your NixOS configuration
imports = [ inputs.bento.nixosModules.bento ];

programs.bento.enable = true;
```

### Ad-hoc

```sh
nix run github:YOUR_NAME/bento -- check
nix profile install github:YOUR_NAME/bento
```

## Runtime dependencies

- `git` — wrapped into the nix package's PATH
- `devenv` — must be on your PATH; used by `bento build` to build the target
  repo's container image
- microsandbox runtime — installed on demand via `bento install`

## Usage

```
bento check    # is the microsandbox runtime installed?
bento install  # install the runtime
bento build    # build the devenv container image for this repo
bento start    # start the box (builds the image when missing)
bento stop     # stop the box
bento enter    # interactive shell in the box
bento agent    # run opencode in the box
bento clean    # remove the box and its opencode volume
```

## Development

```sh
nix develop --no-pure-eval   # same environment as `devenv shell`
```

`--no-pure-eval` lets devenv detect the project directory; `devenv shell`
keeps working as before.
