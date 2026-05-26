use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Owner {
    pub login: String,
    pub id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub owner: Owner,
    pub private: bool,
    /// May be missing on freshly-created empty repos in some API responses.
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Size in KB as reported by GitHub.
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Branch {
    pub name: String,
    pub commit: BranchCommit,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchSummary {
    pub name: String,
    pub commit: GitRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchCommit {
    pub sha: String,
    pub commit: CommitInner,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitInner {
    pub tree: GitRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    pub sha: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub sha: String,
    pub url: String,
    pub tree: Vec<TreeEntry>,
    /// True when GitHub couldn't return the entire tree (limits: ~100k entries
    /// or ~7MB). Callers must fall back to non-recursive listing per subdir.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    /// Git mode string: "100644" file, "100755" exec, "040000" dir, "120000"
    /// symlink, "160000" submodule (gitlink).
    pub mode: String,
    #[serde(rename = "type")]
    pub kind: TreeEntryKind,
    pub sha: String,
    /// Present for blobs, absent for trees/commits.
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TreeEntryKind {
    Blob,
    Tree,
    /// Submodule reference (gitlink).
    Commit,
}
