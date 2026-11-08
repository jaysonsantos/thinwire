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
          # rodio's output device (cpal) links ALSA for in-app voice notes.
          audioLibs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ];
          # In-app video (feature `video`, ADR 0015). The dev shell has the
          # libav plugin for H.264, H.265, and VP9. A release ships only LGPL
          # parts and the decoders of the platform.
          gstLibs = [
            pkgs.gst_all_1.gstreamer
            pkgs.gst_all_1.gst-plugins-base
            pkgs.gst_all_1.gst-plugins-good
            pkgs.gst_all_1.gst-libav
          ];
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
                # `protoc` for the Signal helper: presage and libsignal
                # compile `.proto` files when they build (#248).
                pkgs.protobuf
                pkgs.git-cliff
                # Advisory check of the helper dependencies (ADR 0012 section 4).
                pkgs.cargo-deny
                pkgs.prek
                pkgs.taplo
                # Lossless PNG shrink for the UI snapshots (scripts/update-snapshots.sh).
                pkgs.oxipng
                # App icon files from the SVG (scripts/make-icons.sh).
                pkgs.librsvg
                (pkgs.python3.withPackages (ps: [ ps.pillow ]))
                pkgs.shellcheck
                pkgs.nixfmt
                pkgs.typos
                pkgs.gitleaks
                pkgs.zizmor
                pkgs.editorconfig-checker
              ]
              ++ guiLibs;
              buildInputs = cxxLibs ++ audioLibs ++ gstLibs;

              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              RUST_LOG = "info";
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
                guiLibs ++ cxxLibs ++ glDrivers ++ audioLibs ++ gstLibs
              );
              # The app finds the GStreamer plugins of the dev shell, also when
              # it runs after `nix develop --command` returns.
              GST_PLUGIN_SYSTEM_PATH_1_0 = pkgs.lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstLibs;
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
