//! The project locator, checked from outside the crate: a repository's identity
//! survives whitespace in its path and does not change when it is reached
//! through a symlink.

use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, args: &[&std::ffi::OsStr]) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=elmee test",
            "-c",
            "user.email=elmee@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
#[cfg(unix)]
fn project_locator_preserves_whitespace_and_resolves_symlinks() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join(" repo\n\tюникод ");
    git(scratch.path(), &["init".as_ref(), "-q".as_ref(), repo.as_os_str()]);
    std::fs::create_dir(repo.join("nested")).unwrap();
    assert_eq!(elmee_plugin_paths::project_root(&repo.join("nested")), repo.canonicalize().unwrap());
    let alias = scratch.path().join("alias");
    std::os::unix::fs::symlink(&repo, &alias).unwrap();
    assert_eq!(elmee_plugin_paths::project_root(&alias), repo.canonicalize().unwrap());
}
