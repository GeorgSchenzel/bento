//! The shared nix store: a case-sensitive APFS sparse image (default APFS
//! corrupts stores: terminfo ships both `a/` and `A/`), attached at a fixed
//! mountpoint. `<mountpoint>/nix` is bound as `/nix` into the builder (rw)
//! and every box (ro).

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context as _, Result};
use microsandbox::sandbox::Sandbox;

use crate::util;

/// Image for the builder and every box, pinned (multi-arch index digest).
/// The store's own nix comes from this image (see [seed]); bump it
/// deliberately, as a new image's nix also has to land in the store.
pub const IMAGE: &str =
    "mirror.gcr.io/nixos/nix@sha256:ff480dbb23511aa1c10710aa068ceea5b93fed9bfce5f5379f3a6b07282cbc7b";

/// Sparse: the image only takes the space the store uses, up to this cap.
pub const SIZE: &str = "60g";

const SEED_BOX: &str = "bento-seed";

/// Attach (creating on first use) and seed the store; returns the host path
/// of its `/nix`.
pub async fn ensure() -> Result<PathBuf> {
    let mnt = ensure_attached()?;
    let nix = mnt.join("nix");
    if !nix.join("var/nix/db/db.sqlite").is_file() {
        seed(&mnt).await?;
    }
    Ok(nix)
}

/// The sparse image holding the store.
pub fn image() -> Result<PathBuf> {
    Ok(util::data_dir()?.join("store.sparseimage"))
}

/// Host disk the store takes (the image's allocated size; nix-collect-garbage
/// in the builder frees space inside it, not on the host).
pub fn usage() -> Result<u64> {
    util::disk_usage(&image()?)
}

fn ensure_attached() -> Result<PathBuf> {
    let dir = util::data_dir()?;
    let image = image()?;
    let mnt = dir.join("store");
    if !image.exists() {
        println!(
            "store: creating {} (case-sensitive APFS, max {SIZE})",
            image.display()
        );
        hdiutil(&[
            "create",
            "-size",
            SIZE,
            "-type",
            "SPARSE",
            "-fs",
            "Case-sensitive APFS",
            "-volname",
            "bento-store",
            path_str(&image)?,
        ])?;
    }
    fs::create_dir_all(&mnt).with_context(|| format!("creating {}", mnt.display()))?;
    if !is_mountpoint(&mnt)? {
        // not attached after a reboot/logout; -nobrowse keeps it out of
        // Finder and Spotlight
        hdiutil(&[
            "attach",
            path_str(&image)?,
            "-nobrowse",
            "-mountpoint",
            path_str(&mnt)?,
        ])
        .context("attaching the store (if it is attached elsewhere, `hdiutil detach` it first)")?;
    }
    Ok(mnt)
}

fn is_mountpoint(dir: &Path) -> Result<bool> {
    let parent = dir.parent().context("mountpoint has no parent")?;
    Ok(fs::metadata(dir)?.dev() != fs::metadata(parent)?.dev())
}

fn hdiutil(args: &[&str]) -> Result<()> {
    let out = Command::new("hdiutil")
        .args(args)
        .output()
        .context("running hdiutil")?;
    if !out.status.success() {
        bail!(
            "hdiutil {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("non-UTF-8 path {}", p.display()))
}

/// The image's `/nix` is shadowed by the store mount, so the store starts as
/// a copy of it. Copied into a temp dir and renamed, so a half-done seed is
/// never mistaken for a store.
async fn seed(mnt: &Path) -> Result<()> {
    let tmp = mnt.join("nix.seed");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp)?;
    println!("store: seeding from {IMAGE}...");
    util::remove_sandbox(SEED_BOX).await?;
    let sb = Sandbox::builder(SEED_BOX)
        .image(IMAGE)
        .user("0:0")
        .replace()
        .volume("/seed", |m| m.bind(&tmp).quota(8 * 1024u32))
        .create_detached()
        .await
        .context("creating the seed sandbox")?;
    let res = util::run_streamed(&sb, "seeding the store", "cp -a /nix/. /seed/".into()).await;
    drop(sb);
    util::remove_sandbox(SEED_BOX).await?;
    res?;
    fs::rename(&tmp, mnt.join("nix")).context("activating the seeded store")?;
    println!("store: seeded");
    Ok(())
}
