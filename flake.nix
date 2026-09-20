{
  description = "thinwire: Rust + egui multi-protocol messenger shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          guiLibs = pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.libGL
            pkgs.libxkbcommon
            pkgs.wayland
            pkgs.xorg.libX11
            pkgs.xorg.libXcursor
            pkgs.xorg.libXi
            pkgs.xorg.libXrandr
          ];
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.cmake
              pkgs.pkg-config
              pkgs.git-cliff
              pkgs.prek
              pkgs.taplo
              pkgs.shellcheck
              pkgs.nixfmt
              pkgs.typos
              pkgs.gitleaks
              pkgs.zizmor
              pkgs.editorconfig-checker
            ]
            ++ guiLibs;

            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            RUST_LOG = "info";
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath guiLibs;
          };
        }
      );
    };
}
