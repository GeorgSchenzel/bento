{ pkgs, config, ... }:

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
}
