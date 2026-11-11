//! Who holds which name: presence, claims and what counts as a valid name.

use super::*;

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(super) enum Claim {
    Held,
    Free,
    Unknown,
}

/// Everyone whose process currently holds a name in this mesh.
pub fn peers_of(base: &Path, me: &str) -> std::io::Result<Vec<PeerView>> {
    let entries = match fs::read_dir(base.join("agents")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut peers = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(registration): Option<Registration> = read_json(&path)? else { continue };
        let expected = format!("{}.json", registration.name);
        if !valid_name(&registration.name)
            || path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid mesh registration identity",
            ));
        }
        // Serialize short stale-presence inspection with name acquisition.
        // Otherwise its temporary claim looks like a live peer to a launcher.
        let _gate = delivery_lock(base, &registration.name)?;
        if let Some(claim) = probe_claim(&base.join("locks").join(&registration.name))? {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            drop(claim);
        } else if registration.name != me {
            peers.push(PeerView {
                name: registration.name,
                model: registration.model,
                status: registration.status,
            });
        }
    }
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(peers)
}

#[cfg(test)]
pub(super) fn claim_name(base: &Path, requested: &str) -> (String, Option<File>) {
    match claim_available_name(base, requested) {
        Ok((name, claim)) => (name, Some(claim)),
        Err(_) => (clean_name(requested), None),
    }
}

#[cfg(test)]
pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> bool {
    serialize_record(value).is_ok_and(|bytes| write_atomic(path, &bytes).is_ok())
}

#[cfg(test)]
pub(super) fn claim_of(base: &Path, name: &str) -> Claim {
    match probe_claim(&base.join("locks").join(name)) {
        Ok(Some(_)) => Claim::Free,
        Ok(None) => Claim::Held,
        Err(_) => Claim::Unknown,
    }
}

/// Allocate a readable free name. Only ownership conflicts select another
/// candidate; I/O and binding errors stop allocation immediately.
pub fn claim_available_name(base: &Path, requested: &str) -> std::io::Result<(String, File)> {
    let base_name = clean_name(requested);
    for suffix in 1..100 {
        let candidate = if suffix == 1 {
            base_name.clone()
        } else {
            let suffix = format!("-{suffix}");
            format!("{}{}", &base_name[..base_name.len().min(40 - suffix.len())], suffix)
        };
        if let Some(held) = claim_exact_name(base, &candidate)? {
            return Ok((candidate, held));
        }
    }
    let suffix = unique("");
    let generated = format!("{}{}", &base_name[..base_name.len().min(40 - suffix.len())], suffix);
    let held = claim_exact_name(base, &generated)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "generated mesh name is already claimed")
    })?;
    Ok((generated, held))
}

/// Claim a protocol name without silently choosing a different address.
/// None is a real ownership conflict; storage failures remain errors.
pub fn claim_exact_name(base: &Path, name: &str) -> std::io::Result<Option<File>> {
    if !valid_name(name) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid peer name"));
    }
    let _gate = delivery_lock(base, name)?;
    let Some(mut claim) = hold(&base.join("locks").join(name))? else { return Ok(None) };
    // Rotate while publication is excluded. Never replace the lock inode:
    // existing descriptors must continue to identify the same lifetime lock.
    // Not synced: an incarnation means something only while its lock is held,
    // and a crash releases the lock.
    claim.set_len(0)?;
    claim.write_all(&serde_json::to_vec(&unique("incarnation"))?)?;
    Ok(Some(claim))
}

/// The lifetime a live owner of `name` rotated in when it claimed it. A
/// cooperative fence between one user's own processes, not a credential:
/// capture it once per attachment and never refresh it after a rejection.
pub fn incarnation(base: &Path, name: &str) -> std::io::Result<String> {
    let _gate = delivery_lock(base, name)?;
    let path = base.join("locks").join(name);
    if probe_claim(&path)?.is_some() {
        return Err(std::io::Error::other("mesh name has no active owner"));
    }
    let incarnation: String = serde_json::from_slice(&read_record(&path)?)?;
    if !valid_message_id(&incarnation) {
        return Err(std::io::Error::other("invalid mesh incarnation"));
    }
    Ok(incarnation)
}

/// Hold a file claim. `None` means an actual ownership conflict; I/O errors
/// remain errors and must not be interpreted as an occupied slot or peer.
pub fn hold(path: &Path) -> std::io::Result<Option<File>> {
    let open = || open_lock_record(path);
    let file = match open() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_dir_private(path.parent().unwrap_or(path))?;
            open()?
        }
        result => result?,
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

pub(super) struct ProbeClaim(pub(super) File);

impl Drop for ProbeClaim {
    fn drop(&mut self) {
        // Closing only this descriptor leaves a duplicated/inherited copy locked.
        // A temporary inspection must not extend ownership past its scope.
        let _ = self.0.unlock();
    }
}

pub(super) fn probe_claim(path: &Path) -> std::io::Result<Option<ProbeClaim>> {
    hold(path).map(|claim| claim.map(ProbeClaim))
}

/// A mesh root holds one user's own work: peers are that user's processes and
/// payloads carry their prompts. Records are opened 0600, so the directories
/// mesh creates are 0700 rather than whatever the caller's umask allows. An
/// existing directory keeps its mode; this is not an access-control boundary
/// against the user's own processes.
pub(super) fn create_dir_private(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

pub(super) fn open_lock_record(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        File::from(open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?)
    };
    #[cfg(not(unix))]
    let file = {
        match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(std::io::Error::other("mesh lock is not a regular file"));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        fs::OpenOptions::new().create(true).append(true).open(path)?
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("mesh lock is not a regular file"));
    }
    Ok(file)
}

/// Turn a display name into the portable filename used by the directory binding.
pub fn clean_name(value: &str) -> String {
    let name: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(40)
        .collect();
    if name.is_empty() { "agent".into() } else { name }
}

/// Whether a value is a protocol name rather than a filesystem path.
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

pub fn valid_message_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}
