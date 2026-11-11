#![forbid(unsafe_code)]

//! Crash-safe whole-file replacement.
//!
//! Every durable store in the workspace needs the same three steps: write a
//! sibling, sync it, rename it over the target. Written per store, the steps
//! drift — a missing directory sync loses the rename, a predictable temporary
//! name follows a symlink an attacker left behind. This crate is that one
//! mechanism, and it deliberately knows nothing about paths or formats: it
//! takes a path and bytes, so it stays a boundary primitive rather than
//! persistence policy.

use std::{
    fs,
    io::{Read, Result, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// How durable and how permissive one replacement is.
#[derive(Clone, Copy)]
pub struct Options {
    /// Sync the file and its directory, so the replacement survives power
    /// loss. Off for what is only true while its writer lives — a presence
    /// record, a status — where a sync costs milliseconds and buys nothing:
    /// the replacement is still atomic, just not durable.
    pub durable: bool,
    /// Permission bits for the replacement.
    pub mode: Option<u32>,
    /// Carry the replaced file's permissions onto the replacement.
    pub preserve_mode: bool,
}

impl Default for Options {
    /// Durable by default: a caller content to lose the rename is not here.
    fn default() -> Self {
        Self { durable: true, mode: None, preserve_mode: false }
    }
}

/// Replace `path` with `bytes` as one step, or leave the old contents alone.
pub fn write(path: &Path, bytes: &[u8], options: Options) -> Result<()> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let inherited = || fs::metadata(path).ok().map(|meta| meta.permissions().mode() & 0o777);
    let mode = options.mode.or_else(|| options.preserve_mode.then(inherited).flatten());
    let (temporary, mut file) = create(parent, mode)?;
    let result = (|| {
        file.write_all(bytes)?;
        if options.durable {
            file.sync_all()?;
        }
        fs::rename(&temporary, path)?;
        if options.durable {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Replace `path` with `value` serialized as JSON.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    write(path, &serde_json::to_vec(value).map_err(std::io::Error::other)?, Options::default())
}

/// `create_new` on an unguessable name makes a pre-created symlink a collision
/// rather than something we follow.
fn create(parent: &Path, mode: Option<u32>) -> Result<(PathBuf, fs::File)> {
    for _ in 0..16 {
        let path = parent.join(format!(".elmee-{}.tmp", token()?));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("could not allocate a unique atomic-write file"))
}

fn token() -> Result<String> {
    let mut random = [0_u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("elmee-atomic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn replacement_preserves_the_mode_it_is_asked_to() {
        let root = scratch("mode");
        let target = root.join("value");
        fs::write(&target, "before").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        write(&target, b"after", Options { preserve_mode: true, ..Options::default() }).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "after");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o640);
        write(&target, b"later", Options { mode: Some(0o600), ..Options::default() }).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlink_left_at_a_guessable_temporary_path_is_not_written_through() {
        let root = scratch("symlink");
        let victim = root.join("victim");
        fs::write(&victim, "untouched").unwrap();
        let target = root.join("value.json");
        std::os::unix::fs::symlink(&victim, root.join("value.tmp")).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("value.json.tmp")).unwrap();
        write(&target, b"after", Options::default()).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(&target).unwrap(), "after");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn json_round_trips_and_leaves_no_temporary_behind() {
        let root = scratch("json");
        let target = root.join("nested/record.json");
        write_json(&target, &vec![("name", 3_u32)]).unwrap();
        let read: Vec<(String, u32)> = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(read, vec![("name".to_owned(), 3)]);
        let entries = fs::read_dir(target.parent().unwrap()).unwrap();
        assert_eq!(entries.count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
