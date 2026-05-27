# Changelog

## [0.5.1](https://github.com/abdulrahman1s/github-fs/compare/v0.5.0...v0.5.1) (2026-05-27)


### Bug Fixes

* walk parent_link for passthrough disk paths and raise NOFILE on mount ([1f9ce3e](https://github.com/abdulrahman1s/github-fs/commit/1f9ce3e78d83d5f016e78eb6a0627e61fe3b2518))


### Performance

* **fs:** cut redundant walks and allocations on the FUSE hot path ([4ecdd46](https://github.com/abdulrahman1s/github-fs/commit/4ecdd46c28ce09cde9bb6413dd532d6e2a767e79))

## [0.5.0](https://github.com/abdulrahman1s/github-fs/compare/v0.4.0...v0.5.0) (2026-05-27)


### ⚠ BREAKING CHANGES

* **clone:** one clone per repo, fetch all branches, add fetch_depth
* **cli:** make `ghfs unmount` lazy by default

### Features

* **clone:** one clone per repo, fetch all branches, add fetch_depth ([17dfd79](https://github.com/abdulrahman1s/github-fs/commit/17dfd79f0a39cb3fc7b8e28d8f72f7ac80200e72))
* **clone:** report progress during ensure_clone ([ed01ebe](https://github.com/abdulrahman1s/github-fs/commit/ed01ebe984f1680ebd538db5f042aae84ba082ca))


### Bug Fixes

* **cli:** make `ghfs unmount` lazy by default ([2e0bd44](https://github.com/abdulrahman1s/github-fs/commit/2e0bd44514c8214a5ad51325908b67c09bed8ff9))
* **fs:** force ZERO attr TTL for passthrough inodes ([9f3435a](https://github.com/abdulrahman1s/github-fs/commit/9f3435aaaad444492eb436a71bfb054fcc02fd10))
* **fs:** preserve inode identity across rename ([9990427](https://github.com/abdulrahman1s/github-fs/commit/99904278f38019d0003a70bc29f068afa99d0cc5))


### Documentation

* add example.config.toml with all settings documented ([a33b8dd](https://github.com/abdulrahman1s/github-fs/commit/a33b8dd6811608423fbc131bbed727823d69a880))
* document release-please commit conventions ([d8e8e3f](https://github.com/abdulrahman1s/github-fs/commit/d8e8e3f2902c9a5426a8a5b4c4ecb1a36902369f))
