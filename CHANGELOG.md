# Changelog

## [0.7.0](https://github.com/abdulrahman1s/github-fs/compare/v0.6.0...v0.7.0) (2026-05-29)


### ⚠ BREAKING CHANGES

* **clone:** one clone per repo, fetch all branches, add fetch_depth
* **cli:** make `ghfs unmount` lazy by default

### Features

* **ci:** build aarch64-linux-gnu release artifact ([904df5a](https://github.com/abdulrahman1s/github-fs/commit/904df5a6787c083b87e3bfcfcdf84ea2adf00d1b))
* **cli:** show repo changes during refresh ([45c8fd3](https://github.com/abdulrahman1s/github-fs/commit/45c8fd349ce10653e93d161ea4580c4cbe7c8ae2))
* **clone:** configure origin remotes for materialized repos ([8f95a7d](https://github.com/abdulrahman1s/github-fs/commit/8f95a7df3c6ee3d13dd1955333b9ad0156cb0102))
* **clone:** one clone per repo, fetch all branches, add fetch_depth ([17dfd79](https://github.com/abdulrahman1s/github-fs/commit/17dfd79f0a39cb3fc7b8e28d8f72f7ac80200e72))
* **clone:** report progress during ensure_clone ([ed01ebe](https://github.com/abdulrahman1s/github-fs/commit/ed01ebe984f1680ebd538db5f042aae84ba082ca))
* **fs:** auto-refresh repo list on a configurable interval ([6362f15](https://github.com/abdulrahman1s/github-fs/commit/6362f15b784829764241eef1da7cdba136fbe3a0))
* **fs:** support hard links inside materialized worktrees ([9aa5f91](https://github.com/abdulrahman1s/github-fs/commit/9aa5f9114aac0ceabe47b28db912d4c8e67f3e6c))
* init ([f0cc91e](https://github.com/abdulrahman1s/github-fs/commit/f0cc91eee5108db4e7d44a61d92604146346e1b9))


### Bug Fixes

* **cli:** make `ghfs unmount` lazy by default ([2e0bd44](https://github.com/abdulrahman1s/github-fs/commit/2e0bd44514c8214a5ad51325908b67c09bed8ff9))
* **fs:** force ZERO attr TTL for passthrough inodes ([9f3435a](https://github.com/abdulrahman1s/github-fs/commit/9f3435aaaad444492eb436a71bfb054fcc02fd10))
* **fs:** preserve inode identity across rename ([9990427](https://github.com/abdulrahman1s/github-fs/commit/99904278f38019d0003a70bc29f068afa99d0cc5))
* walk parent_link for passthrough disk paths and raise NOFILE on mount ([1f9ce3e](https://github.com/abdulrahman1s/github-fs/commit/1f9ce3e78d83d5f016e78eb6a0627e61fe3b2518))


### Performance

* **fs:** cut redundant walks and allocations on the FUSE hot path ([4ecdd46](https://github.com/abdulrahman1s/github-fs/commit/4ecdd46c28ce09cde9bb6413dd532d6e2a767e79))


### Documentation

* add example.config.toml with all settings documented ([a33b8dd](https://github.com/abdulrahman1s/github-fs/commit/a33b8dd6811608423fbc131bbed727823d69a880))
* document release-please commit conventions ([d8e8e3f](https://github.com/abdulrahman1s/github-fs/commit/d8e8e3f2902c9a5426a8a5b4c4ecb1a36902369f))

## [0.6.0](https://github.com/abdulrahman1s/github-fs/compare/v0.5.1...v0.6.0) (2026-05-29)


### Features

* **cli:** show repo changes during refresh ([45c8fd3](https://github.com/abdulrahman1s/github-fs/commit/45c8fd349ce10653e93d161ea4580c4cbe7c8ae2))
* **clone:** configure origin remotes for materialized repos ([8f95a7d](https://github.com/abdulrahman1s/github-fs/commit/8f95a7df3c6ee3d13dd1955333b9ad0156cb0102))
* **fs:** support hard links inside materialized worktrees ([9aa5f91](https://github.com/abdulrahman1s/github-fs/commit/9aa5f9114aac0ceabe47b28db912d4c8e67f3e6c))

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
