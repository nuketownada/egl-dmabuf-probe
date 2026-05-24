{
  description = "egl-dmabuf-probe — characterize NVIDIA / Mesa EGL dma-buf import support";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Runtime libraries the binary dlopens / links against. Same set
        # is needed in the devShell so cargo can build, and at runtime
        # so the binary can find libEGL.so.1, libgbm.so.1, etc.
        runtimeLibs = with pkgs; [
          libgbm          # provides libgbm.so (was mesa-libgbm in older nixpkgs)
          libdrm
          libglvnd        # libEGL.so dispatch
          # wayland is a transitive build dep of the `drm` / `gbm`
          # crates' bindings (wayland-sys). We don't use wayland at
          # runtime, but pkg-config needs to find wayland-server.pc at
          # build time.
          wayland
          # At runtime on NVIDIA systems, libEGL.so.1 is loaded from
          # /run/opengl-driver/lib via dlopen — see the LD_LIBRARY_PATH
          # in shellHook below.
        ];

        nativeBuildDeps = with pkgs; [
          pkg-config
          rustc
          cargo
          rustfmt
          clippy
        ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "egl-dmabuf-probe";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = nativeBuildDeps;
          buildInputs = runtimeLibs;
          # The binary dlopens libEGL.so.1 at runtime; on NVIDIA systems
          # that path lives at /run/opengl-driver/lib which we can't
          # patchelf in. Document instead.
          meta = with pkgs.lib; {
            description = "Probe NVIDIA / Mesa EGL implementations for dma-buf import support";
            homepage = "https://github.com/nuketownada/egl-dmabuf-probe";
            license = licenses.mit;
            mainProgram = "egl-dmabuf-probe";
            platforms = platforms.linux;
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };

        devShells.default = pkgs.mkShell {
          name = "egl-dmabuf-probe-dev";
          packages = nativeBuildDeps ++ runtimeLibs ++ (with pkgs; [
            rust-analyzer
            cargo-edit
            cargo-watch
          ]);

          # Make sure pkg-config can find the system libs at build time.
          PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" runtimeLibs;

          # libloading needs to find libEGL.so.1 / libgbm.so.1. On NixOS
          # with NVIDIA, the canonical path is /run/opengl-driver/lib —
          # prepend it so the probe can find the actual NVIDIA libEGL
          # rather than the Mesa one Nix would otherwise serve.
          # libloading needs libEGL.so.1 at runtime. The dispatch layer
          # (libglvnd) provides it; the vendor libs (libEGL_nvidia.so,
          # libEGL_mesa.so, libgbm.so.1) live at /run/opengl-driver/lib
          # on NixOS. Order matters: libglvnd's libEGL.so.1 must dlopen
          # the NVIDIA vendor lib from /run/opengl-driver, so both
          # paths must be visible. Mesa libgbm from /run/opengl-driver
          # is what we want at runtime (matches kernel driver).
          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.libglvnd}/lib:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            echo "egl-dmabuf-probe dev shell."
            echo "  LD_LIBRARY_PATH includes libglvnd dispatch + /run/opengl-driver/lib"
            echo "  cargo build --release  →  ./target/release/egl-dmabuf-probe"
            echo "  cargo run -- -d /dev/dri/renderD129"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
