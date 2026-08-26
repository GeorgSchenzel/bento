{ pkgs, lib, config, ... }:

{
  packages = with pkgs; [
    coreutils
    findutils
    gnugrep
    gnused
    gawk
    git
    curl
    cacert
    procps
    opencode
    zellij
    just
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
  };

  languages.python = {
    enable = true;
    directory = "e2e";
    venv.enable = true;
    uv = {
      enable = true;
      #sync.enable = true;
    };
  };

  env.PATH = "${config.devenv.profile}/bin:/bin:/usr/bin";
  env.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

  # PID 1 in the box is procfileScript via the init shim; with no services
  # devenv-tasks exits at once and takes the VM down. keepalive holds the
  # box open (also required for evaluation: CLI 2.0 nulls task.package
  # when no processes are defined, breaking the image build).
  processes = lib.mkIf config.containers.processes.isBuilding {
    keepalive.exec = "sleep infinity";
  };

  containers.processes.name = "bento";

  # msb never executes the OCI Entrypoint/Cmd (its agent stays PID 1); only
  # `--init <path>` hands PID 1 over. This generic shim execs the devenv
  # entrypoint with the image Cmd — exactly what `docker run` does.
  # copyToRoot lands under the container home /env, not /, so the shim path
  # is /env/init-vibe (as passed by `just box-start`).
  containers.processes.copyToRoot = [
    (pkgs.writeTextFile {
      name = "init-vibe";
      destination = "/init-vibe";
      executable = true;
      text = ''
        #!/bin/bash
        exec ${lib.head config.containers.processes.entrypoint} ${config.procfileScript}
      '';
    })
  ];
}
