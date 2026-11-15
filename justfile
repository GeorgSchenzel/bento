# Unit tests (no VM needed).
test:
    nix develop --no-pure-eval -c cargo test

# bento's VM tests (plan.md phases 1 and 2) under a scratch BENTO_HOME
# (/tmp/bento-e2e). Runs on the host, never in a box; see e2e/run.sh.
e2e:
    e2e/run.sh
