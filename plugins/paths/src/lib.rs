#![forbid(unsafe_code)]

//! Where a project's state lives outside its checkout.
//!
//! One policy, and nothing else: which directory is a project, and where its
//! conversations, inboxes and checkpoints are kept. It is its own crate so the
//! mechanisms that need a path do not inherit the command line that happens
//! to be parsed beside it.

use std::path::{Path, PathBuf};

/// Canonical directory containing this project's persisted conversations.
pub fn session_directory(cwd: &Path) -> PathBuf {
    let root = std::env::var_os("HOME").map_or_else(|| cwd.to_owned(), PathBuf::from).join(".elmee/sessions");
    root.join(format!("{:016x}", workspace_hash(cwd)))
}

/// Stable per-directory key for state kept outside the project.
pub fn workspace_hash(cwd: &Path) -> u64 {
    elmee_core::stable_path_hash(cwd)
}

/// Process coordination state shared by elmee sessions in one project.
/// Where a project's inboxes and runs live: outside the repository, keyed by
/// the project, so nothing of ours is ever committed by accident.
pub fn project_state_directory(cwd: &Path) -> PathBuf {
    if let Some(root) = std::env::var_os("ELMEE_MESH_ROOT") {
        return PathBuf::from(root);
    }
    let root = std::env::var_os("HOME").map_or_else(|| cwd.to_owned(), PathBuf::from);
    root.join(format!(".elmee/projects/{:016x}/mesh-v2", workspace_hash(&project_root(cwd))))
}

/// What a project is: the repository, from wherever inside it and whichever
/// of its worktrees the agent was started — or the directory, when there is
/// no repository.
///
/// Keyed by the working directory, two worktrees of one repository were two
/// projects whose agents could not see each other, which is exactly the
/// arrangement an orchestrator puts agents in. The answer is asked of git
/// once per directory: a mailbox polls this several times a second.
pub fn project_root(cwd: &Path) -> PathBuf {
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>> =
        std::sync::OnceLock::new();
    let mut roots = ROOTS.get_or_init(Default::default).lock().unwrap();
    roots.entry(cwd.to_path_buf()).or_insert_with(|| primary_worktree_of(cwd)).clone()
}

/// Git's primary worktree identity, shared by linked worktrees and subdirectories,
/// or `cwd` outside Git. This is a locator convention, not a wire requirement.
/// The first porcelain record is the primary worktree (also for bare repos).
/// Do not derive this from the parent of the common Git directory: separate
/// repositories may keep their metadata in sibling directories.
fn primary_worktree_of(cwd: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain", "-z"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| primary_worktree(&output.stdout))
        .and_then(|repository| repository.canonicalize().ok())
        .unwrap_or_else(|| cwd.to_owned())
}

fn primary_worktree(output: &[u8]) -> Option<PathBuf> {
    let end = output.iter().position(|byte| *byte == 0)?;
    let path = output[..end].strip_prefix(b"worktree ")?;
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(path))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(path).ok()?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs};

    #[test]
    #[cfg(unix)]
    fn primary_worktree_parses_nul_delimited_paths_without_loss() {
        use std::os::unix::ffi::OsStrExt;
        let path = primary_worktree(b"worktree /repo\n\t\xff \0HEAD 123\0\0").unwrap();
        assert_eq!(path.as_os_str().as_bytes(), b"/repo\n\t\xff ");
        for malformed in
            [b"worktree relative\0".as_slice(), b"worktree /unterminated", b"HEAD 123\0", b"worktree \0"]
        {
            assert!(primary_worktree(malformed).is_none());
        }
    }

    /// One repository is one project, from any of its worktrees or
    /// subdirectories; a directory that is no repository is its own.
    #[test]
    fn a_project_is_the_repository_whichever_worktree_the_agent_is_in() {
        let root = env::temp_dir().join(format!("elmee-project-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (repository, elsewhere) = (root.join("repo"), root.join("plain"));
        fs::create_dir_all(repository.join("src")).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        let git = |args: &[&str], at: &Path| {
            let done = std::process::Command::new("git").args(args).current_dir(at).output().unwrap();
            assert!(done.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&done.stderr));
        };
        git(&["init", "-q"], &repository);
        git(
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "start"],
            &repository,
        );
        let worktree = root.join("wt");
        git(&["worktree", "add", "-q", worktree.to_str().unwrap()], &repository);

        let home = project_state_directory(&repository);
        assert_eq!(project_state_directory(&worktree), home, "a worktree is the same project");
        assert_eq!(project_state_directory(&repository.join("src")), home, "and so is a subdirectory");
        assert_ne!(project_state_directory(&elsewhere), home);
        assert_eq!(project_root(&elsewhere), elsewhere, "no repository: the directory is the project");
        let mut bare_roots = Vec::new();
        for name in ["one.git", "two.git"] {
            let bare = root.join(name);
            git(&["clone", "--bare", "-q", repository.to_str().unwrap(), bare.to_str().unwrap()], &root);
            assert_eq!(project_root(&bare), bare.canonicalize().unwrap());
            bare_roots.push(project_root(&bare));
        }
        assert_ne!(bare_roots[0], bare_roots[1], "sibling bare repositories are independent");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_session_hash_is_stable_and_distinct() {
        assert_eq!(workspace_hash(Path::new("project")), workspace_hash(Path::new("project")));
        assert_ne!(workspace_hash(Path::new("project-a")), workspace_hash(Path::new("project-b")));
    }
}
