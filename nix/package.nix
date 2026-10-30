# The bento CLI, on the platforms microsandbox releases for. Not x86_64-darwin:
# microsandbox needs Apple Silicon on macOS.
{
  lib,
  stdenv,
  rustPlatform,
  fetchurl,
  makeWrapper,
  autoPatchelfHook,
  git,
}:
let
  # The SDK's "prebuilt" feature has build scripts that download the
  # microsandbox runtime (msb + libkrunfw) and the guest agentd
  # binary from GitHub releases at build time — impossible in the nix
  # sandbox. Instead we stage them via fetchurl:
  #   - MSB_AGENTD_PATH embeds agentd (guest PID 1) into bento
  #   - MSB_EMBED_ARTIFACTS_DIR holds the unpacked bundle, which the build
  #     script installs into a throwaway MSB_HOME instead of downloading
  #     it. (Pre-seeding MSB_HOME doesn't do: the script's guard runs
  #     `msb --version`, and the sandbox has no FHS loader on Linux.)
  # Keep in sync with the microsandbox dependency in Cargo.toml:
  # bumping it requires new hashes below.
  msbVersion = "0.7.8";
  release = "https://github.com/superradcompany/microsandbox/releases/download/v${msbVersion}";

  targets = {
    aarch64-darwin = {
      arch = "aarch64";
      os = "darwin";
      bundleHash = "sha256-Xb4Nkhxrb4B9LFn4fgo8OmSEJIcba+5RO4N6jGAKJJM=";
      agentdHash = "sha256-G/yU0eOXQIIBDNwWv534b5ZU1SrChO/ktDn7xnj0JvM=";
    };
    aarch64-linux = {
      arch = "aarch64";
      os = "linux";
      bundleHash = "sha256-apkF/oCn5OP/VUiW73M6PMbtZTkkE1Yxwo3s5loBHP0=";
      agentdHash = "sha256-G/yU0eOXQIIBDNwWv534b5ZU1SrChO/ktDn7xnj0JvM=";
    };
    x86_64-linux = {
      arch = "x86_64";
      os = "linux";
      bundleHash = "sha256-hvn3LcPmOccXW8B5CbS2POQSUXwe+KLhkhFxr1aC/e0=";
      agentdHash = "sha256-GpEbbLheMGEAsQKrS6wnAqz82vkp6n7j6kc0hrBaw0k=";
    };
  };
  target = targets.${stdenv.hostPlatform.system};

  runtimeBundle = fetchurl {
    url = "${release}/microsandbox-${target.os}-${target.arch}.tar.gz";
    hash = target.bundleHash;
  };

  # agentd runs as PID 1 inside the (always Linux) guest
  agentd = fetchurl {
    url = "${release}/agentd-${target.arch}";
    hash = target.agentdHash;
  };

  # The release's msb wants an FHS dynamic loader, which NixOS only has with
  # nix-ld, so on Linux the package ships it patched and points bento at it
  # with MSB_PATH (libkrunfw is found beside it, in ../lib).
  runtime = stdenv.mkDerivation {
    pname = "microsandbox-runtime";
    version = msbVersion;
    src = runtimeBundle;
    sourceRoot = ".";
    nativeBuildInputs = [ autoPatchelfHook ];
    buildInputs = [ stdenv.cc.cc.lib ];
    dontStrip = true; # libkrunfw carries the guest kernel
    installPhase = ''
      mkdir -p $out/bin $out/lib
      cp msb $out/bin/
      cp libkrunfw* $out/lib/
    '';
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
    # the bundle is flat: msb + libkrunfw
    export MSB_EMBED_ARTIFACTS_DIR=$(mktemp -d)
    tar -xzf ${runtimeBundle} -C $MSB_EMBED_ARTIFACTS_DIR
  '';

  # git (repo discovery, in-box identity) is wrapped into PATH; on macOS the
  # microsandbox runtime is managed by `bento install`, and nix on the
  # host is not needed at runtime (it runs in the builder VM).
  postInstall = ''
    wrapProgram $out/bin/bento \
      --prefix PATH : ${lib.makeBinPath [ git ]} \
      ${lib.optionalString stdenv.hostPlatform.isLinux "--set-default MSB_PATH ${runtime}/bin/msb"}
  '';

  meta = {
    description = "Per-checkout microsandbox boxes on a shared nix store";
    mainProgram = "bento";
    platforms = lib.attrNames targets;
  };
}
