use std::{
    fmt, fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootLifetime {
    Retained,
    Scratch,
}

#[derive(Debug)]
pub struct PrivateRoot {
    path: PathBuf,
    lifetime: RootLifetime,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyLeak {
    pub label: String,
    pub location: String,
    pub encoding: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyScan {
    pub files_scanned: u64,
    pub leaks: Vec<PrivacyLeak>,
}

pub fn scan_private_root(
    root: &Path,
    payloads: &[(&str, &[u8])],
) -> Result<PrivacyScan, EstateError> {
    let metadata = root.symlink_metadata().map_err(|error| {
        EstateError(format!(
            "failed to inspect private root {}: {error}",
            root.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EstateError(format!(
            "privacy scan requires a real directory at {}",
            root.display()
        )));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(EstateError(format!(
            "private root {} has mode {mode:o}, expected 700",
            root.display()
        )));
    }
    let mut scan = PrivacyScan {
        files_scanned: 0,
        leaks: Vec::new(),
    };
    scan_directory(root, root, payloads, &mut scan)?;
    Ok(scan)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EstateError(pub String);

impl fmt::Display for EstateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EstateError {}

impl PrivateRoot {
    pub fn create_retained(path: impl Into<PathBuf>) -> Result<Self, EstateError> {
        let path = path.into();
        create_fresh_private_directory(&path)?;
        Ok(Self {
            path,
            lifetime: RootLifetime::Retained,
        })
    }

    pub fn create_scratch(prefix: &str) -> Result<Self, EstateError> {
        validate_component("scratch prefix", prefix)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| EstateError(format!("system clock precedes the Unix epoch: {error}")))?
            .as_nanos();
        for attempt in 0..100_u32 {
            let path = std::env::temp_dir()
                .join(format!("{prefix}-{}-{nonce}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(
                        |error| {
                            EstateError(format!("failed to protect {}: {error}", path.display()))
                        },
                    )?;
                    return Ok(Self {
                        path,
                        lifetime: RootLifetime::Scratch,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(EstateError(format!(
                        "failed to create scratch root {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Err(EstateError(
            "could not allocate a unique scratch root after 100 attempts".to_owned(),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_directory(&self, relative: impl AsRef<Path>) -> Result<PathBuf, EstateError> {
        let path = self.resolve(relative.as_ref())?;
        fs::create_dir_all(&path).map_err(|error| {
            EstateError(format!("failed to create {}: {error}", path.display()))
        })?;
        protect_directory_chain(&self.path, &path)?;
        Ok(path)
    }

    pub fn write_json(
        &self,
        relative: impl AsRef<Path>,
        value: &impl Serialize,
    ) -> Result<PathBuf, EstateError> {
        let path = self.resolve(relative.as_ref())?;
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(EstateError(format!(
                "refusing to replace symbolic link {}",
                path.display()
            )));
        }
        let parent = path
            .parent()
            .ok_or_else(|| EstateError(format!("{} has no parent", path.display())))?;
        fs::create_dir_all(parent).map_err(|error| {
            EstateError(format!("failed to create {}: {error}", parent.display()))
        })?;
        protect_directory_chain(&self.path, parent)?;

        let mut bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| EstateError(format!("failed to serialise evidence: {error}")))?;
        bytes.push(b'\n');
        let temporary = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| EstateError(format!("system clock error: {error}")))?
                .as_nanos()
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                EstateError(format!("failed to create {}: {error}", temporary.display()))
            })?;
        file.write_all(&bytes).map_err(|error| {
            EstateError(format!("failed to write {}: {error}", temporary.display()))
        })?;
        file.sync_all().map_err(|error| {
            EstateError(format!("failed to sync {}: {error}", temporary.display()))
        })?;
        fs::rename(&temporary, &path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            EstateError(format!(
                "failed to publish {} atomically: {error}",
                path.display()
            ))
        })?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            EstateError(format!("failed to protect {}: {error}", path.display()))
        })?;
        Ok(path)
    }

    pub fn scan_for_payloads(
        &self,
        payloads: &[(&str, &[u8])],
    ) -> Result<PrivacyScan, EstateError> {
        scan_private_root(&self.path, payloads)
    }

    pub fn remove(mut self) -> Result<(), EstateError> {
        self.remove_inner()?;
        self.lifetime = RootLifetime::Retained;
        Ok(())
    }

    fn resolve(&self, relative: &Path) -> Result<PathBuf, EstateError> {
        if relative.as_os_str().is_empty() || relative.is_absolute() {
            return Err(EstateError(
                "evidence paths must be non-empty and relative".to_owned(),
            ));
        }
        for component in relative.components() {
            if !matches!(component, Component::Normal(_)) {
                return Err(EstateError(format!(
                    "evidence path {} contains a non-normal component",
                    relative.display()
                )));
            }
        }
        Ok(self.path.join(relative))
    }

    fn remove_inner(&self) -> Result<(), EstateError> {
        if self.path.exists() {
            fs::remove_dir_all(&self.path).map_err(|error| {
                EstateError(format!("failed to remove {}: {error}", self.path.display()))
            })?;
        }
        Ok(())
    }
}

impl Drop for PrivateRoot {
    fn drop(&mut self) {
        if self.lifetime == RootLifetime::Scratch {
            let _ = self.remove_inner();
        }
    }
}

fn create_fresh_private_directory(path: &Path) -> Result<(), EstateError> {
    if path.symlink_metadata().is_ok() {
        return Err(EstateError(format!(
            "fresh evidence root {} already exists",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| EstateError(format!("{} has no parent", path.display())))?;
    reject_existing_symlink_ancestors(parent)?;
    let mut existing_parent = parent;
    while !existing_parent.exists() {
        existing_parent = existing_parent
            .parent()
            .ok_or_else(|| EstateError(format!("{} has no existing ancestor", parent.display())))?;
    }
    fs::create_dir_all(parent)
        .map_err(|error| EstateError(format!("failed to create {}: {error}", parent.display())))?;
    protect_directory_chain(existing_parent, parent)?;
    fs::create_dir(path)
        .map_err(|error| EstateError(format!("failed to create {}: {error}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| EstateError(format!("failed to protect {}: {error}", path.display())))?;
    Ok(())
}

fn reject_existing_symlink_ancestors(path: &Path) -> Result<(), EstateError> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| EstateError(format!("{} has no existing ancestor", path.display())))?;
    }
    let mut current = PathBuf::new();
    for component in existing.components() {
        current.push(component);
        let metadata = current.symlink_metadata().map_err(|error| {
            EstateError(format!("failed to inspect {}: {error}", current.display()))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(EstateError(format!(
                "fresh evidence root parent contains symbolic link {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn protect_directory_chain(root: &Path, leaf: &Path) -> Result<(), EstateError> {
    let relative = leaf
        .strip_prefix(root)
        .map_err(|_| EstateError(format!("{} escapes private root", leaf.display())))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = current.symlink_metadata().map_err(|error| {
            EstateError(format!("failed to inspect {}: {error}", current.display()))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(EstateError(format!(
                "private directory component {} is not a real directory",
                current.display()
            )));
        }
        fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).map_err(|error| {
            EstateError(format!("failed to protect {}: {error}", current.display()))
        })?;
    }
    Ok(())
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    payloads: &[(&str, &[u8])],
    scan: &mut PrivacyScan,
) -> Result<(), EstateError> {
    for entry in fs::read_dir(directory)
        .map_err(|error| EstateError(format!("failed to read {}: {error}", directory.display())))?
    {
        let entry =
            entry.map_err(|error| EstateError(format!("directory entry failed: {error}")))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            EstateError(format!("failed to inspect {}: {error}", path.display()))
        })?;
        if file_type.is_symlink() {
            return Err(EstateError(format!(
                "privacy scan refuses symbolic link {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            let mode = entry
                .metadata()
                .map_err(|error| {
                    EstateError(format!("failed to inspect {}: {error}", path.display()))
                })?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o700 {
                return Err(EstateError(format!(
                    "private directory {} has mode {mode:o}, expected 700",
                    path.display()
                )));
            }
            scan_directory(root, &path, payloads, scan)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let mode = entry
            .metadata()
            .map_err(|error| EstateError(format!("failed to inspect {}: {error}", path.display())))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(EstateError(format!(
                "private evidence file {} has mode {mode:o}; group/other access is forbidden",
                path.display()
            )));
        }
        scan.files_scanned += 1;
        let bytes = fs::read(&path)
            .map_err(|error| EstateError(format!("failed to read {}: {error}", path.display())))?;
        for (label, payload) in payloads {
            if payload.is_empty() {
                continue;
            }
            if contains_bytes(&bytes, payload) {
                scan.leaks.push(PrivacyLeak {
                    label: (*label).to_owned(),
                    location: relative_location(root, &path)?,
                    encoding: "raw".to_owned(),
                });
            }
            let encoded = BASE64.encode(payload);
            if contains_bytes(&bytes, encoded.as_bytes()) {
                scan.leaks.push(PrivacyLeak {
                    label: (*label).to_owned(),
                    location: relative_location(root, &path)?,
                    encoding: "base64".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn relative_location(root: &Path, path: &Path) -> Result<String, EstateError> {
    Ok(path
        .strip_prefix(root)
        .map_err(|_| EstateError(format!("{} escapes privacy root", path.display())))?
        .display()
        .to_string())
}

fn validate_component(field: &str, value: &str) -> Result<(), EstateError> {
    if value.is_empty() || value.contains('/') || value.contains(std::path::MAIN_SEPARATOR) {
        return Err(EstateError(format!("{field} must be one path component")));
    }
    Ok(())
}
