use std::fs::Metadata;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::SystemTime;

use fuser::{FileAttr, FileType};

use crate::fs::inode::InodeKind;

const BLOCK_SIZE: u32 = 4096;

/// Build a [`FileAttr`] for `(ino, kind)`. All timestamps share the same
/// `now` value (typically the mount time) since GitHub doesn't give us
/// per-file mtimes through the tree API.
pub fn build_attr(ino: u64, kind: &InodeKind, now: SystemTime, uid: u32, gid: u32) -> FileAttr {
    let (size, file_type, perm, nlink) = attrs_for(kind);
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: file_type,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

/// Build a [`FileAttr`] from a real on-disk [`Metadata`]. Used by the
/// passthrough path so `stat` on a file inside a materialized worktree
/// reports actual disk size/mtime/perms, not the virtual placeholders the
/// inode was allocated with.
pub fn build_attr_from_metadata(ino: u64, m: &Metadata, uid: u32, gid: u32) -> FileAttr {
    let ft = m.file_type();
    let kind = if ft.is_dir() {
        FileType::Directory
    } else if ft.is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    let perm = (m.permissions().mode() & 0o7777) as u16;
    let size = m.len();
    let atime = m.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
    let mtime = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let ctime = SystemTime::UNIX_EPOCH
        + std::time::Duration::new(m.ctime() as u64, m.ctime_nsec() as u32);
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime,
        mtime,
        ctime,
        crtime: mtime,
        kind,
        perm,
        nlink: m.nlink() as u32,
        uid,
        gid,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn attrs_for(kind: &InodeKind) -> (u64, FileType, u16, u32) {
    match kind {
        InodeKind::Root
        | InodeKind::Owner { .. }
        | InodeKind::Repo { .. }
        | InodeKind::Dir { .. }
        | InodeKind::Submodule { .. } => (0, FileType::Directory, 0o555, 2),
        InodeKind::File {
            size, executable, ..
        } => (
            *size,
            FileType::RegularFile,
            if *executable { 0o555 } else { 0o444 },
            1,
        ),
        InodeKind::Symlink { size, .. } => (*size, FileType::Symlink, 0o777, 1),
    }
}

/// Translate a git tree-entry FileType-like representation into the FUSE
/// `FileType`. Used during readdir so the kernel knows what kind of entry it
/// is without a follow-up getattr.
pub fn kind_to_filetype(kind: &InodeKind) -> FileType {
    match kind {
        InodeKind::Root
        | InodeKind::Owner { .. }
        | InodeKind::Repo { .. }
        | InodeKind::Dir { .. }
        | InodeKind::Submodule { .. } => FileType::Directory,
        InodeKind::File { .. } => FileType::RegularFile,
        InodeKind::Symlink { .. } => FileType::Symlink,
    }
}

/// Git modes:
///   100644 — regular file
///   100755 — executable file
///   120000 — symlink
///   040000 — directory
///   160000 — submodule (gitlink)
pub fn is_executable_mode(mode: &str) -> bool {
    mode == "100755"
}

pub fn is_symlink_mode(mode: &str) -> bool {
    mode == "120000"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::inode::RepoRef;
    use std::time::UNIX_EPOCH;

    fn rref() -> RepoRef {
        RepoRef {
            repo_id: 1,
            owner: "u".into(),
            name: "r".into(),
        }
    }

    #[test]
    fn dirs_have_dir_filetype_perm_555_nlink_2() {
        let a = build_attr(1, &InodeKind::Root, UNIX_EPOCH, 1000, 1000);
        assert_eq!(a.kind, FileType::Directory);
        assert_eq!(a.perm, 0o555);
        assert_eq!(a.nlink, 2);
        assert_eq!(a.size, 0);
    }

    #[test]
    fn regular_file_attrs_use_size_and_default_perm_444() {
        let kind = InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "f".into(),
            blob_sha: "x".into(),
            size: 1234,
            executable: false,
        };
        let a = build_attr(5, &kind, UNIX_EPOCH, 1000, 1000);
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(a.perm, 0o444);
        assert_eq!(a.size, 1234);
        // blocks is ceil(size / 512)
        assert_eq!(a.blocks, 1234u64.div_ceil(512));
    }

    #[test]
    fn executable_file_gets_perm_555() {
        let kind = InodeKind::File {
            repo: rref(),
            branch: "main".into(),
            path: "f".into(),
            blob_sha: "x".into(),
            size: 0,
            executable: true,
        };
        let a = build_attr(5, &kind, UNIX_EPOCH, 1000, 1000);
        assert_eq!(a.perm, 0o555);
    }

    #[test]
    fn symlink_gets_symlink_filetype_and_777() {
        let kind = InodeKind::Symlink {
            repo: rref(),
            branch: "main".into(),
            path: "l".into(),
            blob_sha: "x".into(),
            size: 7,
        };
        let a = build_attr(5, &kind, UNIX_EPOCH, 1000, 1000);
        assert_eq!(a.kind, FileType::Symlink);
        assert_eq!(a.perm, 0o777);
        assert_eq!(a.size, 7);
    }

    #[test]
    fn mode_helpers() {
        assert!(is_executable_mode("100755"));
        assert!(!is_executable_mode("100644"));
        assert!(!is_executable_mode("120000"));
        assert!(is_symlink_mode("120000"));
        assert!(!is_symlink_mode("100644"));
    }
}
