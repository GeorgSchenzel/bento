# The bento CLI. macOS on Apple Silicon only for now: the shared store is an
# APFS sparse image (hdiutil), and microsandbox needs Apple Silicon there.
{
  lib,
  rustPlatform,
  fetchurl,
  makeWrapper,
  git,
}:
let
  # The SDK's "prebuilt" feature has build scripts that download the
  # microsandbox runtime (msb + libkrunfw) and the guest agentd
  # binary from GitHub releases at build time — impossible in the nix
  # sandbox. Instead we stage them via fetchurl:
  #   - MSB_AGENTD_PATH embeds agentd (guest PID 1) into bento
  #   - MSB_HOME pre-seeded with the bundle satisfies the build
  #     script's already-installed guard, skipping its download.
  # Keep in sync with the microsandbox dependency in Cargo.toml:
  # bumping it requires new hashes below.
  msbVersion = "0.7.8";
  release = "https://github.com/superradcompany/microsandbox/releases/download/v${msbVersion}";

  target = {
    arch = "aarch64";
    os = "darwin";
    bundleHash = "sha256-Xb4Nkhxrb4B9LFn4fgo8OmSEJIcba+5RO4N6jGAKJJM=";
  };

  runtimeBundle = fetchurl {
    url = "${release}/microsandbox-${target.os}-${target.arch}.tar.gz";
    hash = target.bundleHash;
  };

  # agentd runs as PID 1 inside the (always Linux) guest
  agentd = fetchurl {
    url = "${release}/agentd-${target.arch}";
    hash = "sha256-G/yU0eOXQIIBDNwWv534b5ZU1SrChO/ktDn7xnj0JvM=";
  };
in
rustPlatform.buildRustPackage {
  pname = "bento";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;

  # builder.rs embeds the tools flake + lock beside src/
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tools/flake.nix
      ../tools/flake.lock
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # crate tests require the microsandbox runtime; behavior is covered
  # by the e2e suite
  doCheck = false;

  nativeBuildInputs = [ makeWrapper ];

  preBuild = ''
    export MSB_AGENTD_PATH=${agentd}

    export MSB_HOME=$(mktemp -d)
    mkdir -p $MSB_HOME/bin $MSB_HOME/lib
    # the bundle is flat: msb + libkrunfw; mirror the build script's
    # extract rules (libkrunfw* → lib/, rest → bin/)
    staging=$(mktemp -d)
    tar -xzf ${runtimeBundle} -C $staging
    for f in "$staging"/*; do
      case $(basename "$f") in
        libkrunfw*) cp -L "$f" $MSB_HOME/lib/ ;;
        *) cp "$f" $MSB_HOME/bin/ && chmod +x $MSB_HOME/bin/$(basename "$f") ;;
      esac
    done
  '';

  # git (repo discovery, in-box identity) is wrapped into PATH; the
  # microsandbox runtime is managed by `bento install`, and nix on the
  # host is not needed at runtime (it runs in the builder VM).
  postInstall = ''
    wrapProgram $out/bin/bento \
      --prefix PATH : ${lib.makeBinPath [ git ]}
  '';

  meta = {
    description = "Per-checkout microsandbox boxes on a shared nix store";
    mainProgram = "bento";
    platforms = [ "aarch64-darwin" ];
  };
}
