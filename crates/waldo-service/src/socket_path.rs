use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use nix::unistd::Uid;

use crate::ServiceError;

pub(crate) fn prepare(path: &Path) -> Result<(), ServiceError> {
    let parent = path.parent().ok_or_else(|| {
        ServiceError::UnsafeSocketPath("socket path has no parent directory".into())
    })?;
    let created = match fs::symlink_metadata(parent) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            true
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceError::UnsafeSocketPath(format!(
            "{} is not a real directory",
            parent.display()
        )));
    }
    let expected = Uid::effective().as_raw();
    if metadata.uid() != expected {
        return Err(ServiceError::UnsafeSocketPath(format!(
            "{} is owned by uid {}, expected {expected}",
            parent.display(),
            metadata.uid()
        )));
    }
    if !created && metadata.mode() & 0o077 != 0 {
        return Err(ServiceError::UnsafeSocketPath(format!(
            "{} is accessible by another user; expected an existing private directory",
            parent.display()
        )));
    }
    remove_stale_socket(path)
}

fn remove_stale_socket(path: &Path) -> Result<(), ServiceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != Uid::effective().as_raw() {
        return Err(ServiceError::UnsafeSocketPath(format!(
            "{} exists but is not a socket owned by this user",
            path.display()
        )));
    }
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    pub fn new(path: PathBuf) -> Result<Self, ServiceError> {
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Never remove a path which replaced the socket after bind. Cleanup is
        // best effort because shutdown must not panic during unwinding/logout.
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
