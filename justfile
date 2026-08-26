tag  := "bento:latest"
name := "bento"
auth := `readlink -f "${HOME}/.local/share/opencode/auth.json"`
cfg  := `readlink -f "${HOME}/.config/opencode/opencode.json"`
cpus := "4"
mem  := "8G"
disk := "16G"
# Guest $HOME (the devenv image sets HOME=/env)
home := "/env"
# Named volume persisting the box's opencode data (sessions, model pin) across restarts
vol  := name + "-opencode"

# Build the image and load it into microsandbox
[group('box')]
box-build:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    devenv container copy processes --registry "oci-archive:$dir/bento-oci.tar:"
    msb load --input "$dir/bento-oci.tar" --tag {{tag}}

# Start the sandbox microVM: repo rw at /workspace, port 8080 published.
# msb never runs the OCI entrypoint; --init hands PID 1 to the generic shim
# (devenv entrypoint + procfileScript = the docker-run equivalent). The shim
# lives at /env — devenv's copyToRoot roots at the container home, not /.
# Opencode data lives on a named volume (auto-created on first run; survives
# --replace and box-rm). auth.json is nested-mounted ro over the volume so the
# token stays in the sops secret on the host and never lands in the volume.
# The mount pins the sops-rendered path at create time — re-run box-start
# (it --replaces) after sops re-renders the secret.
[group('box')]
box-start:
    msb create --replace --name {{name}} -u 0:0 \
      -c {{cpus}} -m {{mem}} --root-disk {{disk}} \
      --init /env/init-vibe \
      --mount-dir "{{justfile_directory()}}:/workspace:rw" \
      --mount-named "{{vol}}:{{home}}/.local/share/opencode" \
      --mount-file "{{auth}}:{{home}}/.local/share/opencode/auth.json:ro" \
      -w /workspace \
      --mkdir "{{home}}/.config/opencode" \
      --mkdir "{{home}}/.local/share/opencode" \
      --copy-file "{{cfg}}:{{home}}/.config/opencode/opencode.json" \
      -p 8080:8080 \
      {{tag}}

# Launch opencode in the sandbox
[group('box')]
box-oc:
    msb exec -t -w /workspace {{name}} -- opencode

# Interactive shell in the sandbox
[group('box')]
box-enter:
    msb exec -t -w /workspace {{name}} -- bash

# Run an arbitrary command in the sandbox
[group('box')]
box-exec +ARGS:
    msb exec -t -w /workspace {{name}} -- {{ARGS}}

# Tail sandbox logs
[group('box')]
box-logs:
    msb logs -f {{name}}

# Stop the sandbox
[group('box')]
box-stop:
    msb stop {{name}}

# Remove the sandbox (the opencode volume survives; see box-reset)
[group('box')]
box-rm:
    msb rm -f {{name}}

# Remove the sandbox AND its persistent opencode data (sessions, model pin)
[group('box')]
box-reset: box-rm
    msb volume rm {{vol}}
