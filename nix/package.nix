# The bento CLI, on Linux (macOS is for later).
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
  # bento depends on the SDK without its `download-binaries` feature, so its
  # build downloads nothing and never installs a runtime into MSB_HOME (by
  # default ~/.microsandbox). The runtime (msb + libkrunfw; msb carries the
  # guest agent) comes from the release bundle, patched for NixOS, and
  # bento finds it through MSB_PATH and MSB_LIBKRUNFW_PATH.
  # Keep in sync with the microsandbox dependency in Cargo.toml: bumping it
  # requires new hashes below.
  msbVersion = "0.7.15";
  libkrunfwVersion = "5.6.1";
  release = "https://github.com/superradcompany/microsandbox/releases/download/v${msbVersion}";

  targets = {
    aarch64-linux = {
      arch = "aarch64";
      bundleHash = "sha256-Gs5eY7tXkwNPDor+fXPGrz5KKwXLW5KHyy2kJSCik9U=";
    };
    x86_64-linux = {
      arch = "x86_64";
      bundleHash = "sha256-N//Hv3srl3qwBPYg9HrkRm8CGpuIDJT0b2l8Vs9pMrc=";
    };
  };
  target = targets.${stdenv.hostPlatform.system};

  runtimeBundle = fetchurl {
    url = "${release}/microsandbox-linux-${target.arch}.tar.gz";
    hash = target.bundleHash;
  };

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

  # builder.rs embeds the tools flake, its lock and bento-run.c
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tools/flake.nix
      ../tools/flake.lock
      ../tools/bento-run.c
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # the unit tests need no VM; the VM tests are `just e2e`
  doCheck = false;

  nativeBuildInputs = [ makeWrapper ];

  # never let a build script touch a real msb home
  preBuild = ''
    export MSB_HOME=$(mktemp -d)
  '';

  # git: the host's identity for the boxes' git config
  postInstall = ''
    wrapProgram $out/bin/bento \
      --prefix PATH : ${lib.makeBinPath [ git ]} \
      --set-default MSB_PATH ${runtime}/bin/msb \
      --set-default MSB_LIBKRUNFW_PATH ${runtime}/lib/libkrunfw.so.${libkrunfwVersion}
  '';

  passthru = { inherit runtime; };

  meta = {
    description = "Local VM sandboxes for agents, on microsandbox and a shared nix store";
    mainProgram = "bento";
    platforms = lib.attrNames targets;
  };
}
