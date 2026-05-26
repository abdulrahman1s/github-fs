use std::path::PathBuf;

use anyhow::{Context, Result};

const PROC_MOUNTS: &str = "/proc/mounts";
const GHFS_FSNAME: &str = "ghfs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhfsMount {
    pub mountpoint: PathBuf,
    pub fstype: String,
    pub options: String,
}

/// Read `/proc/mounts` and return the ghfs entries, sorted by mountpoint
/// length descending so prefix-matching callers can pick the most specific
/// mount first. Shared with `ghfs promote` for resolving FUSE-path args.
pub fn list_ghfs_mounts() -> Result<Vec<GhfsMount>> {
    let raw =
        std::fs::read_to_string(PROC_MOUNTS).with_context(|| format!("reading {PROC_MOUNTS}"))?;
    let mut mounts = parse_proc_mounts(&raw);
    mounts.sort_by_key(|m| std::cmp::Reverse(m.mountpoint.as_os_str().len()));
    Ok(mounts)
}

pub fn run() -> Result<()> {
    let raw =
        std::fs::read_to_string(PROC_MOUNTS).with_context(|| format!("reading {PROC_MOUNTS}"))?;
    let mounts = parse_proc_mounts(&raw);
    if mounts.is_empty() {
        println!("no ghfs mounts found");
        return Ok(());
    }
    for m in &mounts {
        println!("{}\t({}, {})", m.mountpoint.display(), m.fstype, m.options);
    }
    Ok(())
}

/// Extract ghfs mounts from a `/proc/mounts`-formatted string.
///
/// Lines are space-separated: `fsname mountpoint fstype options dump pass`.
/// Mountpoints with spaces are escaped as `\040` per fstab(5); we decode that
/// so the printed path is human-usable.
pub(crate) fn parse_proc_mounts(raw: &str) -> Vec<GhfsMount> {
    raw.lines().filter_map(parse_mount_line).collect()
}

fn parse_mount_line(line: &str) -> Option<GhfsMount> {
    let mut fields = line.split_whitespace();
    let fsname = fields.next()?;
    let mountpoint = fields.next()?;
    let fstype = fields.next()?;
    let options = fields.next()?;

    if fsname != GHFS_FSNAME {
        return None;
    }
    Some(GhfsMount {
        mountpoint: PathBuf::from(unescape_octal(mountpoint)),
        fstype: fstype.to_string(),
        options: options.to_string(),
    })
}

fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && let Some(c) = octal_byte(&bytes[i + 1..i + 4])
        {
            out.push(c as char);
            i += 4;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn octal_byte(triplet: &[u8]) -> Option<u8> {
    let mut v: u16 = 0;
    for &b in triplet {
        let d = (b as char).to_digit(8)? as u16;
        v = v * 8 + d;
    }
    if v <= 0xff { Some(v as u8) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_non_ghfs_lines() {
        let raw = "\
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev,relatime 0 0
";
        assert!(parse_proc_mounts(raw).is_empty());
    }

    #[test]
    fn extracts_single_ghfs_mount() {
        let raw = "\
proc /proc proc rw 0 0
ghfs /home/u/ghfs fuse.ghfs ro,nosuid,nodev,relatime,user_id=1000,group_id=1000,default_permissions 0 0
";
        let mounts = parse_proc_mounts(raw);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/home/u/ghfs"));
        assert_eq!(mounts[0].fstype, "fuse.ghfs");
        assert!(mounts[0].options.starts_with("ro,"));
    }

    #[test]
    fn extracts_multiple_ghfs_mounts() {
        let raw = "\
ghfs /mnt/a fuse.ghfs ro 0 0
tmpfs /tmp tmpfs rw 0 0
ghfs /mnt/b fuse.ghfs ro 0 0
";
        let mounts = parse_proc_mounts(raw);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/mnt/a"));
        assert_eq!(mounts[1].mountpoint, PathBuf::from("/mnt/b"));
    }

    #[test]
    fn decodes_octal_escapes_in_mountpoint() {
        // Space in a mountpoint is escaped as \040 by the kernel.
        let raw = "ghfs /home/u/my\\040ghfs fuse.ghfs ro 0 0\n";
        let mounts = parse_proc_mounts(raw);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/home/u/my ghfs"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let raw = "ghfs onlyfourfields\nghfs /mnt fuse.ghfs ro 0 0\n";
        let mounts = parse_proc_mounts(raw);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/mnt"));
    }

    #[test]
    fn does_not_match_substring_of_fsname() {
        // "ghfsfoo" must not be mistaken for "ghfs".
        let raw = "ghfsfoo /mnt fuse ro 0 0\n";
        assert!(parse_proc_mounts(raw).is_empty());
    }
}
