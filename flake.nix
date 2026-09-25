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
            pkgs.vulkan-loader
            pkgs.wayland
            pkgs.xorg.libX11
            pkgs.xorg.libXcursor
            pkgs.xorg.libXi
            pkgs.xorg.libXrandr
          ];
          # Static TDLib (`telegram-tdlib`) links and loads the LLVM C++ runtime.
          cxxLibs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.llvmPackages.libcxx ];
          # Nix libglvnd does not see host GL drivers on non-NixOS Linux.
          # Point it at nixpkgs mesa (AMD / Intel). NVIDIA hosts need nixGL.
          glDrivers = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.mesa ];
        in
        {
          default = pkgs.mkShell (
            {
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
              buildInputs = cxxLibs;

              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              RUST_LOG = "info";
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (guiLibs ++ cxxLibs ++ glDrivers);
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
              __EGL_VENDOR_LIBRARY_DIRS = "${pkgs.mesa}/share/glvnd/egl_vendor.d";
              LIBGL_DRIVERS_PATH = "${pkgs.mesa}/lib/dri";
              # lavapipe: software Vulkan. Snapshot tests prefer a CPU adapter.
              VK_DRIVER_FILES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
            }
          );
        }
      );
    };
}
