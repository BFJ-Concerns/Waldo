//! SHA-256 identities for evidence, computed by the system's sha256sum.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// The SHA-256 of `bytes`.
pub(crate) fn sha256_bytes(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("sha256sum did not expose standard input")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed for in-memory evidence: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_sha256(&output.stdout)
}

/// The SHA-256 of the file at `path`.
pub(crate) fn sha256_file(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_sha256(&output.stdout)
}

fn parse_sha256(output: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let digest = String::from_utf8(output.to_vec())?
        .split_whitespace()
        .next()
        .ok_or("sha256sum returned no digest")?
        .to_owned();
    if !waldo_verification::is_sha256_digest(&digest) {
        return Err(format!("sha256sum returned an invalid digest {digest:?}").into());
    }
    Ok(digest)
}
