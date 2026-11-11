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

use std::path::{Path, PathBuf};

use tokio::process::Command;

#[derive(Clone)]
pub struct Snapshots {
    git_dir: PathBuf,
    cwd: PathBuf,
    roots: Vec<Snapshots>,
}

/// Recording the tree costs three `git` runs, and `git` spends about eight
/// milliseconds starting up before it does two tenths of a millisecond of
/// work — so a snapshot taken inline was forty milliseconds of pure process
/// overhead in front of every prompt. Nothing may *write* until it lands, but
/// the model call does not write, so `begin` overlaps the two and hands the
/// loop an [`elmee_core::Pending`] to settle before the first tool.
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

    async fn git(&self, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.cwd)
            .args(args)
            .output()
            .await
            .ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
        self.git(&["init", "--quiet"]).await.map(|_| ())
    }

    /// Content identity of everything a snapshot would restore. Unlike a
    /// timestamp or the user's HEAD this includes uncommitted edits, new files,
    /// formatter output, and changes made by Bash or subagents.
    pub async fn fingerprint(&self) -> Option<String> {
        let mut trees = Vec::new();
        for root in std::iter::once(self).chain(&self.roots) {
            trees.push((root.cwd.clone(), root.fingerprint_one().await?));
        }
        if self.roots.is_empty() {
            return trees.pop().map(|(_, tree)| tree);
        }
        Some(format!("{:016x}", elmee_core::stable_hash(serde_json::to_vec(&trees).ok()?)))
    }

    async fn fingerprint_one(&self) -> Option<String> {
        self.ensure_store().await?;
        self.git(&["add", "--all"]).await?;
        self.git(&["write-tree"]).await
    }

    /// Start recording the tree, to be settled before anything writes to it.
    pub fn begin(&self, summary: &str) -> Pending {
        let (snapshots, summary) = (self.clone(), summary.to_owned());
        let handle = tokio::spawn(async move { snapshots.take(&summary).await });
        Pending::new(async move { handle.await.ok().flatten() })
    }

    /// Record the tree as it stands, labelled with the prompt about to run.
    /// Returns nothing when git is unavailable — snapshots are a convenience,
    /// never a precondition for working.
    pub async fn take(&self, summary: &str) -> Option<String> {
        if self.roots.is_empty() {
            return self.take_one(summary).await;
        }
        let mut commits = Vec::new();
        for root in std::iter::once(self).chain(&self.roots) {
            commits.push((root.cwd.clone(), root.take_one(summary).await?));
        }
        Some(format!("roots-v1:{}", serde_json::to_string(&commits).ok()?))
    }

    async fn take_one(&self, summary: &str) -> Option<String> {
        self.ensure_store().await?;
        self.git(&["add", "--all"]).await?;
        let summary = summary.lines().next().unwrap_or("(no prompt)").chars().take(72).collect::<String>();
        // Our own identity, because this is our own store rather than the
        // user's repository — and because a machine with no global git identity
        // failed every commit with "Author identity unknown", silently, which
        // is what a failed snapshot is by design. `/rewind` was simply always
        // empty there, and so was anything else built on snapshots.
        self.git(&[
            "-c",
            "user.name=elmee",
            "-c",
            "user.email=elmee@localhost",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            &summary,
        ])
        .await?;
        self.git(&["rev-parse", "HEAD"]).await
    }

    /// Put the working tree back to a snapshot: files changed since are
    /// restored, files created since are removed. Ignored files are left
    /// alone.
    ///
    /// The tree as it stands is recorded first, for two reasons that are one
    /// step. A file created after the last snapshot is unknown to the store,
    /// and what the store does not know it cannot remove — so a rewind used to
    /// leave new files behind unless something happened to have scanned the
    /// tree in between. And removing files is the one thing here that cannot
    /// be taken back from the project alone, so the state being left is itself
    /// a snapshot to return to: nothing a rewind removes is gone.
    pub async fn restore(&self, commit: &str) -> Result<(), String> {
        let roots: Vec<&Snapshots> = std::iter::once(self).chain(&self.roots).collect();
        let commits: Vec<(PathBuf, String)> = match commit.strip_prefix("roots-v1:") {
            Some(encoded) => {
                serde_json::from_str(encoded).map_err(|_| "invalid workspace checkpoint".to_owned())?
            }
            None if self.roots.is_empty() => vec![(self.cwd.clone(), commit.into())],
            None => {
                return Err("checkpoint does not include the current additional workspace directories".into());
            }
        };
        if commits.len() != roots.len()
            || commits.iter().zip(&roots).any(|((path, commit), root)| {
                path != &root.cwd
                    || !matches!(commit.len(), 40 | 64)
                    || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err("checkpoint workspace directories differ; nothing was changed".into());
        }
        for ((_, commit), root) in commits.iter().zip(&roots) {
            root.git(&["cat-file", "-e", &format!("{commit}^{{commit}}")])
                .await
                .ok_or_else(|| "checkpoint is unavailable; nothing was changed".to_owned())?;
        }
        self.take("before rewind").await.ok_or_else(|| {
            "cannot record the working tree before restoring; nothing was changed".to_owned()
        })?;
        for ((_, commit), root) in commits.iter().zip(&roots) {
            root.git(&["read-tree", "-u", "--reset", commit]).await.ok_or_else(|| {
                format!("cannot restore snapshot {} in {}", &commit[..8], root.cwd.display())
            })?;
        }
        Ok(())
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
        let Some(checkpoint) = snapshots.take("both roots").await else { return };
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
    async fn snapshots_round_trip_a_working_tree() {
        let cwd = std::env::temp_dir().join(format!("elmee-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "original").unwrap();

        let snapshots = Snapshots::new(&cwd);
        let _ = std::fs::remove_dir_all(&snapshots.git_dir);
        let Some(commit) = snapshots.take("first prompt").await else { return }; // no git: skip
        let original_tree = snapshots.fingerprint().await.unwrap();

        // A later turn rewrites the file and adds another; restoring undoes both.
        std::fs::write(cwd.join("a.txt"), "changed by bash").unwrap();
        std::fs::write(cwd.join("b.txt"), "added by bash").unwrap();
        assert_ne!(snapshots.fingerprint().await.as_deref(), Some(original_tree.as_str()));
        snapshots.take("second prompt").await.unwrap();
        snapshots.restore(&commit).await.unwrap();

        assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "original");
        assert!(!cwd.join("b.txt").exists());
        // History is kept, not rewound — and the state left behind joins it.
        let log = snapshots.git(&["log", "--format=%s"]).await.unwrap();
        assert_eq!(log.lines().collect::<Vec<_>>(), ["before rewind", "second prompt", "first prompt"]);

        // The case the scan above used to hide: a file created since the last
        // snapshot, with nothing having looked at the tree in between.
        std::fs::write(cwd.join("c.txt"), "created and never scanned").unwrap();
        snapshots.restore(&commit).await.unwrap();
        assert!(!cwd.join("c.txt").exists(), "a file made after the snapshot goes with the rewind");
        // And it is not gone: the rewind recorded what it removed.
        let undo = snapshots.git(&["rev-parse", "HEAD"]).await.unwrap();
        snapshots.restore(&undo).await.unwrap();
        assert_eq!(std::fs::read_to_string(cwd.join("c.txt")).unwrap(), "created and never scanned");
        let _ = std::fs::remove_dir_all(&snapshots.git_dir);
        let _ = std::fs::remove_dir_all(cwd);
    }
}
