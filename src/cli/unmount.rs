use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Result, anyhow, bail};
use tracing::info;

/// Unmount a ghfs mountpoint by shelling out to `fusermount3 -u`.
///
/// We delegate rather than re-implementing the unmount syscall because
/// `fusermount3` is the only path that works for an unprivileged user-mount
/// installed via the standard FUSE setuid helper.
///
/// When `lazy` is true we pass `-z` as well so the kernel detaches the
/// mount immediately even if it is still in use; the mount is freed once
/// the last `cwd`/open handle goes away. We don't default to lazy because
/// it can mask real bugs (e.g. ghfs threads still holding the FS).
pub fn run(path: PathBuf, lazy: bool) -> Result<()> {
    let canonical = path.canonicalize().map_err(|e| {
        anyhow!(
            "cannot resolve mountpoint {}: {e}",
            path.display()
        )
    })?;

    if !canonical.is_dir() {
        bail!("mountpoint is not a directory: {}", canonical.display());
    }

    info!(mountpoint = %canonical.display(), lazy, "unmounting");
    let output = fusermount(&canonical, lazy)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        if !lazy && is_busy(trimmed) {
            bail!(
                "{} is busy — another process has it open or `cd`'d into it.\n\
                 Find the holder with `fuser -vm {}` (or `lsof +f -- {}`), \
                 or rerun with `ghfs unmount --lazy {}` to detach now and \
                 free the mount when the last reference goes away.",
                canonical.display(),
                canonical.display(),
                canonical.display(),
                canonical.display(),
            );
        }
        let detail = if trimmed.is_empty() {
            String::new()
        } else {
            format!(": {trimmed}")
        };
        bail!(
            "fusermount3 exited with status {}{detail}",
            output.status,
        );
    }
    info!(mountpoint = %canonical.display(), "unmounted");
    println!("unmounted: {}", canonical.display());
    Ok(())
}

fn fusermount(path: &Path, lazy: bool) -> Result<Output> {
    let mut cmd = Command::new("fusermount3");
    if lazy {
        cmd.arg("-uz");
    } else {
        cmd.arg("-u");
    }
    cmd.arg(path)
        .output()
        .map_err(|e| anyhow!("failed to invoke fusermount3: {e} (is fuse3 installed?)"))
}

/// fusermount3 prints messages like:
///   "fusermount3: failed to unmount /path: Device or resource busy"
/// across versions. Match on the OS-level error string so we don't depend
/// on the exact prefix.
fn is_busy(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("device or resource busy") || lower.contains("target is busy")
}

#[cfg(test)]
mod tests {
    use super::is_busy;

    #[test]
    fn detects_fusermount3_busy_message() {
        assert!(is_busy(
            "fusermount3: failed to unmount /home/me/ghfs: Device or resource busy"
        ));
    }

    #[test]
    fn detects_umount_busy_message() {
        assert!(is_busy("umount: /mnt/x: target is busy."));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!is_busy("fusermount3: entry for /mnt/x not found in /etc/mtab"));
        assert!(!is_busy(""));
    }
}
