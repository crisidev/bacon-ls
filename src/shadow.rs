//! Hardlinked shadow workspace used for live "as you type" diagnostics.
//!
//! cargo can only check files on disk. To surface diagnostics for unsaved
//! editor buffers, we mirror the workspace into
//! `<workspace>/target/bacon-ls-live/shadow/` using **hardlinks** (one inode
//! per file, no data copy), then on `did_change` we replace the hardlink for
//! the dirty file with a real file containing the buffer content. Cargo runs
//! against the shadow with its own target dir at
//! `<workspace>/target/bacon-ls-live/target/` so it can't deadlock with the
//! user's regular `cargo build` against the real `target/`.
//!
//! What gets mirrored: everything that wouldn't be excluded by `git status`
//! — the `ignore` crate (same engine `ripgrep` uses) walks the workspace
//! respecting `.gitignore`, `.ignore`, `.git/info/exclude`, the global
//! gitignore, and the hidden-file filter. Plus a hardcoded skip for our own
//! `target/bacon-ls-live/` so the shadow can't recursively mirror itself if
//! a workspace happens not to gitignore `target/`.
//!
//! Path remapping back to the real workspace happens at the cargo invocation
//! site via `--remap-path-prefix`, so diagnostics published to the editor
//! still carry the user's real source paths.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ShadowWorkspace {
    real_root: PathBuf,
    shadow_root: PathBuf,
    target_dir: PathBuf,
}

impl ShadowWorkspace {
    /// Build (or rebuild) the shadow tree for `real_root`. Wipes any stale
    /// shadow contents from a previous LSP session — keeping them risks
    /// resurrecting a forgotten dirty buffer — but **preserves** the live
    /// target dir so cargo's incremental cache survives across restarts.
    pub(crate) async fn build(real_root: PathBuf) -> std::io::Result<Self> {
        let live_root = real_root.join("target").join("bacon-ls-live");
        let shadow_root = live_root.join("shadow");
        let target_dir = live_root.join("target");

        if tokio::fs::try_exists(&shadow_root).await? {
            tokio::fs::remove_dir_all(&shadow_root).await?;
        }
        tokio::fs::create_dir_all(&shadow_root).await?;
        tokio::fs::create_dir_all(&target_dir).await?;

        // Off-thread the filesystem walk: `ignore`'s walker is sync, and on
        // a large workspace we'd otherwise stall the LSP runtime.
        let real = real_root.clone();
        let shadow = shadow_root.clone();
        let live = live_root.clone();
        tokio::task::spawn_blocking(move || mirror_blocking(&real, &shadow, &live))
            .await
            .map_err(std::io::Error::other)??;

        Ok(Self {
            real_root,
            shadow_root,
            target_dir,
        })
    }

    pub(crate) fn real_root(&self) -> &Path {
        &self.real_root
    }

    pub(crate) fn shadow_root(&self) -> &Path {
        &self.shadow_root
    }

    pub(crate) fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// Translate a real-workspace path into its position inside the shadow.
    /// Errors if `real_path` is not inside the workspace root.
    pub(crate) fn shadow_path_for(&self, real_path: &Path) -> std::io::Result<PathBuf> {
        let rel = real_path.strip_prefix(&self.real_root).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "path {} is not inside workspace {}",
                    real_path.display(),
                    self.real_root.display(),
                ),
            )
        })?;
        Ok(self.shadow_root.join(rel))
    }

    /// Replace the shadow entry for `real_path` with a real file containing
    /// `content`. Implemented as write-tmp-then-rename so the rename is atomic
    /// and so the real file's inode is **not** modified — only the directory
    /// entry inside the shadow.
    pub(crate) async fn write_dirty(&self, real_path: &Path, content: &str) -> std::io::Result<()> {
        let shadow_path = self.shadow_path_for(real_path)?;
        if let Some(parent) = shadow_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = shadow_path.with_extension("bacon-ls-tmp");
        tokio::fs::write(&tmp, content).await?;
        tokio::fs::rename(&tmp, &shadow_path).await?;
        Ok(())
    }

    /// Restore the shadow entry for `real_path` back to a hardlink of the
    /// on-disk file. Used after `did_save` / `did_close` so subsequent cargo
    /// runs see the saved content.
    pub(crate) async fn restore_link(&self, real_path: &Path) -> std::io::Result<()> {
        let shadow_path = self.shadow_path_for(real_path)?;
        if tokio::fs::try_exists(&shadow_path).await? {
            tokio::fs::remove_file(&shadow_path).await?;
        }
        if let Some(parent) = shadow_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::hard_link(real_path, &shadow_path).await?;
        Ok(())
    }
}

/// Sync mirror walk. Runs inside `spawn_blocking`. Uses the `ignore` crate so
/// gitignored / hidden / `.ignore`'d files are skipped, and adds a hardcoded
/// guard against descending into our own shadow dir (`live_root`).
fn mirror_blocking(real: &Path, shadow: &Path, live_root: &Path) -> std::io::Result<()> {
    use ignore::WalkBuilder;

    let walker = WalkBuilder::new(real)
        // `ignore`'s defaults already enable .gitignore / .ignore /
        // .git/info/exclude / global gitignore + hidden-file filtering, but
        // make the intent explicit so a future audit doesn't have to read
        // the crate's source.
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        // Crucially: don't require the workspace to be a git repo before
        // applying .gitignore rules. Users with jj / hg / no VCS still write
        // .gitignore files, and the rules are exactly what we want.
        .require_git(false)
        .parents(true)
        .filter_entry({
            let live_root = live_root.to_path_buf();
            move |e| !e.path().starts_with(&live_root)
        })
        .build();

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(?err, "skipping entry while mirroring shadow");
                continue;
            }
        };
        let path = entry.path();
        // The walker yields the root itself first; skip it (the shadow root
        // already exists).
        let rel = match path.strip_prefix(real) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        let shadow_path = shadow.join(rel);
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if ft.is_dir() {
            std::fs::create_dir_all(&shadow_path)?;
        } else if ft.is_file() {
            if let Some(parent) = shadow_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // If shadow_path already exists from a partially-completed previous
            // run, `hard_link` would error with EEXIST. We removed the shadow
            // root at the top of `build`, so this shouldn't happen, but be
            // defensive: replace if present.
            if std::fs::metadata(&shadow_path).is_ok() {
                std::fs::remove_file(&shadow_path)?;
            }
            std::fs::hard_link(path, &shadow_path)?;
        }
        // Symlinks: skipped in v1. Rare in Rust workspaces and the semantics
        // (target inside vs outside the workspace, broken vs valid) need more
        // thought than they're worth right now.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    fn mk_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_build_mirrors_files_via_hardlink_same_inode() {
        let tmp = TempDir::new().unwrap();
        let lib_rs = tmp.path().join("src/lib.rs");
        mk_file(&lib_rs, "// content");

        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();
        let shadow_lib_rs = shadow.shadow_root().join("src/lib.rs");
        assert!(shadow_lib_rs.exists());
        assert_eq!(
            inode(&lib_rs),
            inode(&shadow_lib_rs),
            "shadow file must hardlink to the real file (same inode)"
        );
    }

    #[tokio::test]
    async fn test_build_excludes_gitignored_paths() {
        let tmp = TempDir::new().unwrap();
        // Put `target/` in .gitignore explicitly so the test doesn't rely on
        // the user's global gitignore. Also gitignore a custom build output.
        mk_file(&tmp.path().join(".gitignore"), "target/\nbuild-output/\n");
        mk_file(&tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"");
        mk_file(&tmp.path().join("src/lib.rs"), "// real");
        mk_file(&tmp.path().join("target/release/big.rlib"), "binary");
        mk_file(&tmp.path().join("build-output/snapshot.bin"), "junk");

        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();

        assert!(shadow.shadow_root().join("Cargo.toml").exists());
        assert!(shadow.shadow_root().join("src/lib.rs").exists());
        // gitignored content must NOT be mirrored.
        assert!(!shadow.shadow_root().join("target").exists());
        assert!(!shadow.shadow_root().join("build-output").exists());
    }

    #[tokio::test]
    async fn test_build_excludes_hidden_dirs() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join("Cargo.toml"), "x");
        mk_file(&tmp.path().join(".git/HEAD"), "ref:");
        mk_file(&tmp.path().join(".idea/workspace.xml"), "<x/>");

        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();
        assert!(!shadow.shadow_root().join(".git").exists());
        assert!(!shadow.shadow_root().join(".idea").exists());
    }

    #[tokio::test]
    async fn test_build_does_not_recurse_into_its_own_live_dir() {
        // A workspace that doesn't gitignore target/ at all (unusual but
        // possible): we must still skip target/bacon-ls-live/ to avoid the
        // shadow recursively containing itself.
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join("Cargo.toml"), "x");
        mk_file(&tmp.path().join("src/lib.rs"), "x");
        // No .gitignore at all → ignore crate's defaults still skip `.git`
        // (hidden) and respect any global gitignore, but won't filter
        // target/. We populate target/bacon-ls-live before calling build to
        // simulate a prior run.
        mk_file(
            &tmp.path().join("target/bacon-ls-live/shadow/should-not-recurse.rs"),
            "x",
        );

        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();

        // The shadow root must not contain a nested `target/bacon-ls-live/`
        // (which would mean we recursed into our own output).
        assert!(
            !shadow.shadow_root().join("target/bacon-ls-live").exists(),
            "shadow must not recurse into its own live dir"
        );
    }

    #[tokio::test]
    async fn test_build_wipes_stale_shadow_from_prior_session() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        mk_file(&tmp.path().join("src/lib.rs"), "fresh");
        // Simulate leftover shadow with a stale dirty buffer.
        mk_file(
            &tmp.path().join("target/bacon-ls-live/shadow/src/lib.rs"),
            "stale dirty content",
        );
        mk_file(
            &tmp.path().join("target/bacon-ls-live/shadow/src/gone.rs"),
            "deleted in real workspace",
        );

        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();

        assert!(
            !shadow.shadow_root().join("src/gone.rs").exists(),
            "stale shadow entries from prior runs must not survive a rebuild"
        );
        let mirrored = std::fs::read_to_string(shadow.shadow_root().join("src/lib.rs")).unwrap();
        assert_eq!(mirrored, "fresh", "stale dirty content must be replaced");
    }

    #[tokio::test]
    async fn test_build_preserves_target_dir_for_cache_reuse() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        mk_file(&tmp.path().join("src/lib.rs"), "x");
        let cache_marker = tmp.path().join("target/bacon-ls-live/target/CACHE_MARKER");
        mk_file(&cache_marker, "previous build artifacts");

        ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();
        assert!(
            cache_marker.exists(),
            "live target dir must persist across rebuilds so cargo can reuse its cache"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_write_dirty_replaces_hardlink_with_distinct_inode() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        let lib_rs = tmp.path().join("src/lib.rs");
        mk_file(&lib_rs, "saved");
        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();
        let shadow_lib_rs = shadow.shadow_root().join("src/lib.rs");
        assert_eq!(inode(&lib_rs), inode(&shadow_lib_rs));

        shadow.write_dirty(&lib_rs, "dirty buffer").await.unwrap();

        assert_ne!(
            inode(&lib_rs),
            inode(&shadow_lib_rs),
            "write_dirty must break the hardlink"
        );
        assert_eq!(
            std::fs::read_to_string(&shadow_lib_rs).unwrap(),
            "dirty buffer",
            "shadow now carries dirty content"
        );
        assert_eq!(
            std::fs::read_to_string(&lib_rs).unwrap(),
            "saved",
            "real file must be untouched"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_restore_link_reverts_to_hardlink() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        let lib_rs = tmp.path().join("src/lib.rs");
        mk_file(&lib_rs, "saved");
        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();
        let shadow_lib_rs = shadow.shadow_root().join("src/lib.rs");

        shadow.write_dirty(&lib_rs, "dirty").await.unwrap();
        assert_ne!(inode(&lib_rs), inode(&shadow_lib_rs));

        shadow.restore_link(&lib_rs).await.unwrap();
        assert_eq!(
            inode(&lib_rs),
            inode(&shadow_lib_rs),
            "restore_link must hardlink shadow back to the real file"
        );
        assert_eq!(std::fs::read_to_string(&shadow_lib_rs).unwrap(), "saved");
    }

    #[tokio::test]
    async fn test_write_dirty_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        mk_file(&tmp.path().join("src/lib.rs"), "x");
        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();

        // A new file the user just created in the editor — parent shadow dir
        // doesn't exist yet (file was added after the initial mirror).
        let new_file = tmp.path().join("src/new/nested/mod.rs");
        std::fs::create_dir_all(new_file.parent().unwrap()).unwrap();
        std::fs::write(&new_file, "real").unwrap();

        shadow.write_dirty(&new_file, "in-editor draft").await.unwrap();
        let shadow_path = shadow.shadow_root().join("src/new/nested/mod.rs");
        assert_eq!(std::fs::read_to_string(&shadow_path).unwrap(), "in-editor draft");
    }

    #[tokio::test]
    async fn test_shadow_path_for_outside_workspace_errors() {
        let tmp = TempDir::new().unwrap();
        mk_file(&tmp.path().join(".gitignore"), "target/\n");
        mk_file(&tmp.path().join("src/lib.rs"), "x");
        let shadow = ShadowWorkspace::build(tmp.path().to_path_buf()).await.unwrap();

        let outside = std::path::PathBuf::from("/etc/passwd");
        let err = shadow.shadow_path_for(&outside).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
