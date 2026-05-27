{
  description = "Mount GitHub repositories as a read-only FUSE filesystem on Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      binName = "ghfs";
      releaseOwner = "abdulrahman1s";
      releaseRepo = "github-fs";
      # FUSE bindings (libfuse3) are Linux-only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      prebuiltTargets = {
        x86_64-linux = "x86_64-unknown-linux-gnu";
        aarch64-linux = "aarch64-unknown-linux-gnu";
      };
      forAllSystems = lib.genAttrs systems;
      mkPkgs = system: import nixpkgs { inherit system; };
      source = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          let
            name = baseNameOf path;
          in
          lib.cleanSourceFilter path type
          && !(type == "directory" && name == "target")
          && !(name == "result" || lib.hasPrefix "result-" name);
      };
      mkPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = cargoToml.package.name;
          version = cargoToml.package.version;

          src = source;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.installShellFiles
          ];
          # openssl is pulled in by libgit2's `https` feature on git2; without
          # it, the openssl-sys build script can't find openssl.pc.
          buildInputs = [
            pkgs.fuse3
            pkgs.openssl
          ];

          # `ghfs completions <shell>` writes a clap-generated script to
          # stdout; emit them into the standard nix locations
          # (bash-completion / site-functions / vendor_completions.d).
          postInstall = ''
            installShellCompletion --cmd ${binName} \
              --bash <($out/bin/${binName} completions bash) \
              --zsh  <($out/bin/${binName} completions zsh) \
              --fish <($out/bin/${binName} completions fish)
          '';

          meta = {
            description = "Mount GitHub repositories as a read-only FUSE filesystem on Linux";
            mainProgram = binName;
            platforms = lib.platforms.linux;
          };
        };
      mkPrebuiltPackage =
        pkgs:
        {
          version ? cargoToml.package.version,
          hash,
          owner ? releaseOwner,
          repo ? releaseRepo,
          target ?
            prebuiltTargets.${pkgs.stdenv.hostPlatform.system}
              or (throw "No ${binName} prebuilt target for ${pkgs.stdenv.hostPlatform.system}"),
          url ? null,
        }:
        let
          system = pkgs.stdenv.hostPlatform.system;
          releaseVersion = lib.removePrefix "v" version;
          releaseTag = "v${releaseVersion}";
          archiveUrl =
            if url != null then
              url
            else if target == null then
              throw "No ${binName} prebuilt target was provided for ${system}"
            else
              "https://github.com/${owner}/${repo}/releases/download/${releaseTag}/${binName}-${releaseTag}-${target}.tar.gz";
        in
        pkgs.stdenvNoCC.mkDerivation {
          pname = "${binName}-prebuilt";
          version = releaseVersion;

          src = pkgs.fetchurl {
            url = archiveUrl;
            hash = if hash == "" then lib.fakeHash else hash;
          };

          nativeBuildInputs = [
            pkgs.autoPatchelfHook
            pkgs.installShellFiles
          ];
          # ghfs links against libssl/libcrypto via git2's https feature;
          # autoPatchelfHook needs openssl on the host to wire them up.
          buildInputs = [
            pkgs.fuse3
            pkgs.openssl
            pkgs.stdenv.cc.cc.lib
          ];

          sourceRoot = ".";
          dontConfigure = true;
          dontBuild = true;

          installPhase = ''
            runHook preInstall
            install -Dm0755 ${binName} "$out/bin/${binName}"
            runHook postInstall
          '';

          # autoPatchelfHook registers itself in postFixupHooks, which
          # runs *after* the user `postFixup` variable — so completions
          # can't be generated in postFixup (the binary isn't patched
          # yet). installCheckPhase runs after fixupPhase completes, by
          # which point autoPatchelf has wired up the interpreter and
          # rpath, so the binary is executable.
          doInstallCheck = true;
          installCheckPhase = ''
            runHook preInstallCheck
            installShellCompletion --cmd ${binName} \
              --bash <($out/bin/${binName} completions bash) \
              --zsh  <($out/bin/${binName} completions zsh) \
              --fish <($out/bin/${binName} completions fish)
            runHook postInstallCheck
          '';

          meta = {
            description = "Prebuilt ${binName} binary from GitHub Releases";
            mainProgram = binName;
            platforms = builtins.attrNames prebuiltTargets;
          };
        };
    in
    {
      lib.mkPrebuiltPackage = mkPrebuiltPackage;

      packages = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;
          pkg = mkPackage pkgs;
        in
        {
          github-fs = pkg;
          default = pkg;
        }
      );

      apps = forAllSystems (
        system:
        let
          app = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/${binName}";
            meta.description = "Run ${binName}";
          };
        in
        {
          ghfs = app;
          default = app;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = mkPkgs system;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              pkg-config
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
            ];

            buildInputs = with pkgs; [
              fuse3
              openssl
            ];

            # Help bindgen / build scripts find headers for libfuse3 and
            # openssl (the latter is pulled in by git2's `https` feature).
            PKG_CONFIG_PATH = "${pkgs.fuse3.dev}/lib/pkgconfig:${pkgs.openssl.dev}/lib/pkgconfig";

            # rust-analyzer needs the std source to resolve `core`/`std`;
            # nixpkgs' bare `rustc` doesn't ship it, so point r-a at the
            # matching rustLibSrc derivation.
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        }
      );

      overlays.default =
        final: _prev:
        let
          system = final.stdenv.hostPlatform.system;
        in
        {
          github-fs = self.packages.${system}.default;
        };

      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.github-fs;
          system = pkgs.stdenv.hostPlatform.system;
          prebuiltPackage = mkPrebuiltPackage pkgs {
            inherit (cfg.prebuilt)
              hash
              owner
              repo
              target
              url
              version
              ;
          };
          package = if cfg.prebuilt.enable then prebuiltPackage else cfg.package;
        in
        {
          options.programs.github-fs = {
            enable = lib.mkEnableOption "ghfs read-only GitHub FUSE filesystem";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${system}.default;
              defaultText = "self.packages.<system>.default";
              description = "The github-fs package to install.";
            };

            prebuilt = {
              enable = lib.mkEnableOption "installing ghfs from a prebuilt GitHub Release binary instead of building from source";

              version = lib.mkOption {
                type = lib.types.str;
                default = cargoToml.package.version;
                defaultText = "the version in Cargo.toml";
                description = "Release version to fetch, with or without the leading v.";
              };

              hash = lib.mkOption {
                type = lib.types.str;
                default = "";
                example = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
                description = "SRI hash for the release tarball. This is required when prebuilt.enable is true.";
              };

              owner = lib.mkOption {
                type = lib.types.str;
                default = releaseOwner;
                description = "GitHub owner or organization that hosts the release assets.";
              };

              repo = lib.mkOption {
                type = lib.types.str;
                default = releaseRepo;
                description = "GitHub repository that hosts the release assets.";
              };

              target = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = prebuiltTargets.${system} or null;
                defaultText = "the GitHub release target for the host platform";
                description = "Release artifact target triple to fetch.";
              };

              url = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                example = "https://github.com/abdulrahman1s/github-fs/releases/download/v0.1.0/ghfs-v0.1.0-x86_64-unknown-linux-gnu.tar.gz";
                description = "Full release tarball URL. When set, owner, repo, version, and target are only used for package metadata.";
              };
            };

            autoMount = {
              enable = lib.mkEnableOption "a systemd user service that mounts the ghfs filesystem on login";

              mountPath = lib.mkOption {
                type = lib.types.str;
                example = "/home/you/ghfs";
                description = "Absolute path that the service will mount the filesystem at. Created on start if missing.";
              };

              cacheDir = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                example = "/home/you/.cache/ghfs";
                description = "Override the cache directory used by the mount; null leaves it at ghfs's default.";
              };

              tokenFile = lib.mkOption {
                type = lib.types.nullOr lib.types.path;
                default = null;
                example = "/run/secrets/ghfs-token";
                description = ''
                  Path to a file whose contents are loaded via systemd
                  EnvironmentFile=. The file must define GHFS_TOKEN=...
                  (one line, mode 0600). When null, the service inherits
                  whatever environment systemd's user manager has.
                '';
              };

              extraArgs = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [ ];
                example = [ "--log-level" "ghfs=info" ];
                description = "Extra arguments appended to `ghfs mount <mountPath>` in the service's ExecStart.";
              };

              restartSec = lib.mkOption {
                type = lib.types.either lib.types.int lib.types.str;
                default = 5;
                description = "systemd RestartSec= for the ghfs user service.";
              };
            };
          };

          config = lib.mkIf cfg.enable (lib.mkMerge [
            {
              assertions = [
                {
                  assertion = !cfg.prebuilt.enable || cfg.prebuilt.hash != "";
                  message = "programs.github-fs.prebuilt.hash must be set when programs.github-fs.prebuilt.enable is true.";
                }
                {
                  assertion = !cfg.prebuilt.enable || cfg.prebuilt.url != null || cfg.prebuilt.target != null;
                  message = "No ghfs prebuilt target is known for ${system}; set programs.github-fs.prebuilt.target or programs.github-fs.prebuilt.url.";
                }
                {
                  assertion = !cfg.autoMount.enable || cfg.autoMount.mountPath != "";
                  message = "programs.github-fs.autoMount.mountPath must be set when programs.github-fs.autoMount.enable is true.";
                }
              ];

              environment.systemPackages = [ package ];

              # ghfs uses libfuse3 via the kernel's FUSE device.
              programs.fuse.userAllowOther = lib.mkDefault true;
            }

            (lib.mkIf cfg.autoMount.enable {
              # User services are scoped to the logged-in user (no root mount).
              # The mount lives at $XDG_RUNTIME_DIR of that user's session.
              systemd.user.services.ghfs = {
                description = "Mount GitHub repositories as a read-only FUSE filesystem";
                after = [ "network-online.target" ];
                wants = [ "network-online.target" ];
                wantedBy = [ "default.target" ];

                # The mount needs fusermount3 in PATH for ExecStop and for
                # fuser's own teardown calls.
                path = [
                  package
                  pkgs.fuse3
                  pkgs.coreutils
                ];

                serviceConfig = {
                  Type = "simple";
                  ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p ${lib.escapeShellArg cfg.autoMount.mountPath}";
                  ExecStart = lib.concatStringsSep " " (
                    [
                      "${package}/bin/ghfs"
                      "mount"
                      (lib.escapeShellArg cfg.autoMount.mountPath)
                    ]
                    ++ (lib.optionals (cfg.autoMount.cacheDir != null) [
                      "--cache-dir"
                      (lib.escapeShellArg cfg.autoMount.cacheDir)
                    ])
                    ++ (map lib.escapeShellArg cfg.autoMount.extraArgs)
                  );
                  ExecStop = "${pkgs.fuse3}/bin/fusermount3 -u ${lib.escapeShellArg cfg.autoMount.mountPath}";
                  Restart = "on-failure";
                  RestartSec = toString cfg.autoMount.restartSec;
                  # SIGINT triggers ghfs's clean-unmount path; SIGTERM is the
                  # default and works too, but SIGINT matches the foreground
                  # Ctrl-C behavior exactly.
                  KillSignal = "SIGINT";
                  TimeoutStopSec = "20";
                } // lib.optionalAttrs (cfg.autoMount.tokenFile != null) {
                  EnvironmentFile = toString cfg.autoMount.tokenFile;
                };
              };
            })
          ]);
        };
    };
}
