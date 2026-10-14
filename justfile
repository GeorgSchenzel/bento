# Build and install `bento` into ~/.cargo/bin (replaces any older bento there).
install:
    cargo install --path . --locked

# Remove `bento` from ~/.cargo/bin.
uninstall:
    cargo uninstall bento
