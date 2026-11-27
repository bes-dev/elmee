#![forbid(unsafe_code)]

//! Point-in-time snapshots of the working tree, so a turn's file changes can
//! be undone.
//!
//! Intercepting the edit tools would only see `write` and `edit`; everything
//! bash does — `rm`, `mv`, `sed -i`, installs, formatters, codegen — and every
//! subagent write would be invisible. Snapshotting *state* instead of
//! *calls* is complete by construction.
//!
//! The snapshots live in a git directory outside the project, so the user's
//! own history, index, and stash are never touched, and the project's
//! `.gitignore` still applies (build output stays out). A project that is not
//! a git repository works the same way.
//!
//! A snapshot is that store's index, copied: `git add --all` has by then put
//! every file's content into the store, and the index is the list of which
//! content was where. Committing it as well was two more `git` runs, each
//! several milliseconds of starting up, in front of the first tool of every
//! turn — to give a name to something a file copy already names.

use std::path::{Path, PathBuf};

use tokio::process::Command;

#[derive(Clone)]
pub struct Snapshots {
    git_dir: PathBuf,
    cwd: PathBuf,
    roots: Vec<Snapshots>,
}

pub use elmee_core::Pending;

impl Snapshots {
    pub fn new(cwd: &Path) -> Self {
        let root = std::env::var_os("HOME").map_or_else(|| cwd.to_owned(), PathBuf::from);
        let hash = elmee_plugin_paths::workspace_hash(cwd);
        Self {
            git_dir: root.join(format!(".elmee/snapshots/{hash:016x}")),
            cwd: cwd.to_owned(),
            roots: Vec::new(),
        }
    }

    pub fn with_roots(mut self, mut roots: Vec<PathBuf>) -> Self {
        roots.sort();
        roots.dedup();
        self.roots =
            roots.into_iter().filter(|root| root != &self.cwd).map(|root| Self::new(&root)).collect();
        self
    }

    /// Run git against the store, reading and writing `index` as its index:
    /// the store's own to look at the tree, a snapshot to say what it held.
    async fn git(&self, index: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.cwd)
            .args(args)
            .env("GIT_INDEX_FILE", index)
            .output()
            .await
            .ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn index(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    fn snapshot(&self, name: &str) -> PathBuf {
        self.git_dir.join("elmee-snapshots").join(name)
    }

    /// Make sure the store exists before git is asked to use it.
    ///
    /// `git init --git-dir=X` creates X but not the directory above it, so on
    /// a machine where `~/.elmee/snapshots` had never been made every single
    /// snapshot failed with "Invalid path" — and because a failed snapshot is
    /// deliberately silent, `/rewind` simply had nothing in it, forever.
    async fn ensure_store(&self) -> Option<()> {
        if self.git_dir.exists() {
            return Some(());
        }
        std::fs::create_dir_all(&self.git_dir).ok()?;
        self.git(&self.index(), &["init", "--quiet"]).await.map(|_| ())
    }

    /// Bring the store's index up to the tree as it stands.
    async fn scan(&self) -> Option<()> {
        self.ensure_store().await?;
        self.git(&self.index(), &["add", "--all"]).await.map(|_| ())
    }

    /// Content identity of everything a snapshot would restore. Unlike a
    /// timestamp or the user's HEAD this includes uncommitted edits, new files,
    /// formatter output, and changes made by Bash or subagents.
    pub async fn fingerprint(&self) -> Option<String> {
        let mut trees = Vec::new();
        for root in std::iter::once(self).chain(&self.roots) {
            root.scan().await?;
            trees.push((root.cwd.clone(), root.git(&root.index(), &["write-tree"]).await?));
        }
        if self.roots.is_empty() {
            return trees.pop().map(|(_, tree)| tree);
        }
        Some(format!("{:016x}", elmee_core::stable_hash(serde_json::to_vec(&trees).ok()?)))
    }

    /// Recording the tree, as work owed before anything writes to it and not
    /// begun until then.
    pub fn begin(&self) -> Pending {
        let snapshots = self.clone();
        Pending::new(async move { snapshots.take().await })
    }

    /// Record the tree as it stands. Returns nothing when git is unavailable
    /// — snapshots are a convenience, never a precondition for working.
    pub async fn take(&self) -> Option<String> {
        if self.roots.is_empty() {
            return self.take_one().await;
        }
        let mut snapshots = Vec::new();
        for root in std::iter::once(self).chain(&self.roots) {
            snapshots.push((root.cwd.clone(), root.take_one().await?));
        }
        Some(format!("roots-v1:{}", serde_json::to_string(&snapshots).ok()?))
    }

    async fn take_one(&self) -> Option<String> {
        self.scan().await?;
        let index = self.index();
        // A tree with nothing in it leaves no index behind, and a snapshot of
        // nothing is still one: restoring it removes what was made since.
        if !index.exists() {
            self.git(&index, &["read-tree", "--empty"]).await?;
        }
        let bytes = std::fs::read(&index).ok()?;
        let name = format!("{:016x}", elmee_core::stable_hash(bytes.iter().copied()));
        let kept = self.snapshot(&name);
        if !kept.exists() {
            // Renamed into place, so a snapshot is whole or absent.
            let partial = kept.with_extension(std::process::id().to_string());
            std::fs::create_dir_all(kept.parent()?).ok()?;
            std::fs::write(&partial, bytes).ok()?;
            std::fs::rename(partial, kept).ok()?;
        }
        Some(name)
    }

    /// Put the working tree back to a snapshot: files changed since are
    /// restored, files created since are removed. Ignored files are left
    /// alone. Returns the snapshot of the state it left.
    ///
    /// The tree as it stands is recorded first, for two reasons that are one
    /// step. A file created after the last snapshot is unknown to the store,
    /// and what the store does not know it cannot remove — so a rewind used to
    /// leave new files behind unless something happened to have scanned the
    /// tree in between. And removing files is the one thing here that cannot
    /// be taken back from the project alone, so the state being left is itself
    /// a snapshot to return to: nothing a rewind removes is gone.
    pub async fn restore(&self, snapshot: &str) -> Result<String, String> {
        let roots: Vec<&Snapshots> = std::iter::once(self).chain(&self.roots).collect();
        let snapshots: Vec<(PathBuf, String)> = match snapshot.strip_prefix("roots-v1:") {
            Some(encoded) => {
                serde_json::from_str(encoded).map_err(|_| "invalid workspace checkpoint".to_owned())?
            }
            None if self.roots.is_empty() => vec![(self.cwd.clone(), snapshot.into())],
            None => {
                return Err("checkpoint does not include the current additional workspace directories".into());
            }
        };
        if snapshots.len() != roots.len()
            || snapshots.iter().zip(&roots).any(|((path, _), root)| path != &root.cwd)
        {
            return Err("checkpoint workspace directories differ; nothing was changed".into());
        }
        // The name is joined to a path, so it is checked to be a name.
        if snapshots.iter().zip(&roots).any(|((_, name), root)| {
            !name.bytes().all(|byte| byte.is_ascii_hexdigit()) || !root.snapshot(name).is_file()
        }) {
            return Err("checkpoint is unavailable; nothing was changed".into());
        }
        let left = self.take().await.ok_or_else(|| {
            "cannot record the working tree before restoring; nothing was changed".to_owned()
        })?;
        for ((_, name), root) in snapshots.iter().zip(&roots) {
            let failed = || format!("cannot restore snapshot {name} in {}", root.cwd.display());
            let tree = root.git(&root.snapshot(name), &["write-tree"]).await.ok_or_else(failed)?;
            root.git(&root.index(), &["read-tree", "-u", "--reset", &tree]).await.ok_or_else(failed)?;
        }
        Ok(left)
    }
}

impl elmee_core::Plugin for Snapshots {
    fn id(&self) -> &'static str {
        "snapshots"
    }

    fn declare(self, registry: &mut elmee_core::Registrar) -> elmee_core::Result<()> {
        registry.provide_service(std::rc::Rc::new(self));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoints_cover_additional_roots_and_refuse_a_changed_root_set() {
        let directory = std::env::temp_dir().join(elmee_core::unique("multi-root-snapshot"));
        let (main, extra) = (directory.join("main"), directory.join("extra"));
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(main.join("a"), "original main").unwrap();
        std::fs::write(extra.join("b"), "original extra").unwrap();
        let snapshots = Snapshots::new(&main).with_roots(vec![extra.clone()]);
        let Some(checkpoint) = snapshots.take().await else { return };
        let before = snapshots.fingerprint().await.unwrap();
        std::fs::write(extra.join("b"), "changed extra").unwrap();
        assert_ne!(
            snapshots.fingerprint().await.as_deref(),
            Some(before.as_str()),
            "evidence must become stale after an extra-root edit"
        );
        std::fs::write(main.join("a"), "changed main").unwrap();
        std::fs::write(extra.join("new"), "new file").unwrap();
        let wrong = Snapshots::new(&main);
        assert!(wrong.restore(&checkpoint).await.is_err());
        assert_eq!(std::fs::read_to_string(main.join("a")).unwrap(), "changed main");
        snapshots.restore(&checkpoint).await.unwrap();
        assert_eq!(std::fs::read_to_string(main.join("a")).unwrap(), "original main");
        assert_eq!(std::fs::read_to_string(extra.join("b")).unwrap(), "original extra");
        assert!(!extra.join("new").exists());
        for root in std::iter::once(&snapshots).chain(&snapshots.roots) {
            let _ = std::fs::remove_dir_all(&root.git_dir);
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn a_directory_with_nothing_in_it_is_a_snapshot_to_return_to() {
        let cwd = std::env::temp_dir().join(elmee_core::unique("empty-snapshot"));
        std::fs::create_dir_all(&cwd).unwrap();
        let snapshots = Snapshots::new(&cwd);
        let Some(empty) = snapshots.take().await else { return }; // no git: skip
        std::fs::write(cwd.join("made.txt"), "by the first turn").unwrap();
        snapshots.restore(&empty).await.unwrap();
        assert!(!cwd.join("made.txt").exists());
        let _ = std::fs::remove_dir_all(&snapshots.git_dir);
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn snapshots_round_trip_a_working_tree() {
        let cwd = std::env::temp_dir().join(format!("elmee-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "original").unwrap();

        let snapshots = Snapshots::new(&cwd);
        let _ = std::fs::remove_dir_all(&snapshots.git_dir);
        let Some(first) = snapshots.take().await else { return }; // no git: skip
        let original_tree = snapshots.fingerprint().await.unwrap();

        // A later turn rewrites the file and adds another; restoring undoes both.
        std::fs::write(cwd.join("a.txt"), "changed by bash").unwrap();
        std::fs::write(cwd.join("b.txt"), "added by bash").unwrap();
        assert_ne!(snapshots.fingerprint().await.as_deref(), Some(original_tree.as_str()));
        snapshots.restore(&first).await.unwrap();

        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "original");
        assert!(!cwd.join("b.txt").exists());

        // The case a scan in between used to hide: a file created since the
        // last snapshot, with nothing having looked at the tree in between.
        std::fs::write(cwd.join("c.txt"), "created and never scanned").unwrap();
        let left = snapshots.restore(&first).await.unwrap();
        assert!(!cwd.join("c.txt").exists(), "a file made after the snapshot goes with the rewind");
        // And it is not gone: the rewind recorded what it removed.
        snapshots.restore(&left).await.unwrap();
        assert_eq!(std::fs::read_to_string(cwd.join("c.txt")).unwrap(), "created and never scanned");
        assert!(snapshots.restore("../index").await.is_err(), "a snapshot is named, never pathed");
        let _ = std::fs::remove_dir_all(&snapshots.git_dir);
        let _ = std::fs::remove_dir_all(cwd);
    }
}
