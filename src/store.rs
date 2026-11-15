//! The instance's shared nix store: a plain dir (Linux; macOS, which needs a
//! case-sensitive image, is for later). `<store>/nix` is bound as `/nix`
//! into the builder (rw) and every box (ro).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use microsandbox::sandbox::Sandbox;

use crate::instance::Instance;
use crate::names;

/// Image for the builder and every box, pinned (multi-arch index digest).
/// The store's own nix comes from this image (see [seed]); bump it
/// deliberately, as a new image's nix also has to land in the store.
pub const IMAGE: &str =
    "mirror.gcr.io/nixos/nix@sha256:ff480dbb23511aa1c10710aa068ceea5b93fed9bfce5f5379f3a6b07282cbc7b";

/// Attach (creating on first use) and seed the instance's store; returns
/// the host path of its `/nix`.
pub async fn ensure(instance: &Instance, prefix: &str) -> Result<PathBuf> {
    let mnt = ensure_attached(instance)?;
    let nix = mnt.join("nix");
    if !nix.join("var/nix/db/db.sqlite").is_file() {
        seed(&mnt, &names::msb_seed(prefix)).await?;
    }
    Ok(nix)
}

fn ensure_attached(instance: &Instance) -> Result<PathBuf> {
    let dir = instance.store_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The image's `/nix` is shadowed by the store mount, so the store starts as
/// a copy of it. Copied into a temp dir and renamed, so a half-done seed is
/// never mistaken for a store.
async fn seed(mnt: &Path, seed_box: &str) -> Result<()> {
    let tmp = mnt.join("nix.seed");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp)?;
    eprintln!("bento: store: seeding from {IMAGE}...");
    crate::util::remove_sandbox(seed_box).await?;
    let sb = Sandbox::builder(seed_box)
        .image(IMAGE)
        .user("0:0")
        .replace()
        .volume("/seed", |m| m.bind(&tmp).quota(8 * 1024u32))
        .create_detached()
        .await
        .context("creating the seed sandbox")?;
    let res =
        crate::util::run_streamed(&sb, "seeding the store", "cp -a /nix/. /seed/".into()).await;
    drop(sb);
    crate::util::remove_sandbox(seed_box).await?;
    res?;
    fs::rename(&tmp, mnt.join("nix")).context("activating the seeded store")?;
    eprintln!("bento: store: seeded");
    Ok(())
}
