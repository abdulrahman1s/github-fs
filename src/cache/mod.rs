pub mod blobs;
pub mod clones;
pub mod errors;
pub mod meta;

pub use blobs::BlobStore;
pub use clones::{CloneStore, default_remote_base};
pub use errors::CacheError;
pub use meta::{BranchHead, MetaCache};

/// Etag cache key for the authenticated user's owned-repos list. Shared
/// between the FUSE layer (which sends If-None-Match) and the `refresh`
/// subcommand (which clears the etag so the next fetch forces a 200).
pub const OWNED_USER_REPOS_ETAG_KEY: &str = "owned_user_repos_v1";

/// Etag cache key for the broader "all visible repos" list — fetched when the
/// `owners` config is `All` or a `List`. URL differs from the owned-only
/// variant (no `affiliation=owner` query param), so the ETag space is
/// distinct.
pub const ALL_USER_REPOS_ETAG_KEY: &str = "all_user_repos_v1";
