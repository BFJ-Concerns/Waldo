use std::path::{Path, PathBuf};

use serde::Serialize;

pub(crate) fn check_destination(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!("refusing to replace symbolic link {}", path.display()).into());
    }
    Ok(())
}

/// Write owner-only JSON without changing the containing directory's permissions.
pub(crate) fn write(
    path: PathBuf,
    value: &impl Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    check_destination(&path)?;
    Ok(waldo_verification::publish_private_json(&path, value)?)
}
