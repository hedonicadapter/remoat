{
  description = "remoat — phone→desktop remote control daemon (touchpad + push-to-talk)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        inherit (pkgs) lib stdenv;

        # Tools the daemon shells out to at runtime. Linux/Wayland needs ydotool +
        # wl-clipboard; grim captures the touchpad's live cursor-area preview via
        # wlr-screencopy (Hyprland and other wlroots compositors); openssl is not required
        # by the Rust build (rcgen) but harmless.
        # wlr-randr is the screen-bounds fallback for compositors without their own IPC
        # (niri and Hyprland are queried directly); it was referenced by screen.rs long
        # before it was ever actually on PATH, which silently forced the 1920x1080 default.
        #
        # curl + tar (with bzip2) are for `remoat --fetch-model`, which downloads the speech
        # model at runtime rather than baking ~122MB into the store path. On darwin both are
        # system binaries; on Linux they must be declared like everything else remoat shells
        # out to. Learned the hard way with wlr-randr: a tool referenced in code is not a tool
        # on PATH.
        runtimeTools = lib.optionals stdenv.isLinux
          (with pkgs; [ ydotool wl-clipboard xdg-utils grim wlr-randr curl gnutar bzip2 ]);

        src = builtins.path {
          name = "remoat-src";
          path = ./.;
          filter = path: type:
            let b = baseNameOf path; in
            !(b == ".venv" || b == "certs" || b == "result" || b == ".git"
              || b == "__pycache__" || b == "target");
        };

        remoat = pkgs.rustPlatform.buildRustPackage {
          pname = "remoat";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;

          # cmake + perl: rustls pulls aws-lc-sys, whose build invokes cmake (and perl).
          nativeBuildInputs = [ pkgs.makeWrapper pkgs.cmake pkgs.perl ];

          # The streaming speech recognizer. nixpkgs ships sherpa-onnx as a shared library on
          # unix, which is exactly what the `shared` cargo feature wants.
          buildInputs = [ pkgs.sherpa-onnx pkgs.onnxruntime ];

          # MANDATORY, not an optimisation: sherpa-onnx-sys's build.rs otherwise DOWNLOADS a
          # prebuilt archive from GitHub, which a sandboxed nix build cannot do (and must not).
          # Pointing it at the nixpkgs library makes the build hermetic and keeps the crate and
          # the system library on the same version.
          SHERPA_ONNX_LIB_DIR = "${pkgs.sherpa-onnx}/lib";

          # Bundle the PWA and point the binary at it; put runtime tools on its PATH.
          postInstall = ''
            mkdir -p $out/share/remoat
            cp -r web $out/share/remoat/web
            wrapProgram $out/bin/remoat \
              --set-default REMOAT_WEB_DIR $out/share/remoat/web \
              ${lib.optionalString (runtimeTools != [])
                "--prefix PATH : ${lib.makeBinPath runtimeTools}"}
          '';

          meta = {
            description = "Phone→desktop remote control daemon";
            mainProgram = "remoat";
            platforms = lib.platforms.unix;
          };
        };
      in
      {
        packages.default = remoat;
        packages.remoat = remoat;

        apps.default = { type = "app"; program = lib.getExe remoat; };

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.rustc pkgs.cargo pkgs.pkg-config pkgs.cmake pkgs.sherpa-onnx ]
            ++ runtimeTools;
          # Same reason as the package: without this, a plain `cargo build` in the dev shell
          # tries to fetch the prebuilt archive instead of using the one nix already provides.
          SHERPA_ONNX_LIB_DIR = "${pkgs.sherpa-onnx}/lib";
          LD_LIBRARY_PATH = lib.optionalString stdenv.isLinux "${pkgs.sherpa-onnx}/lib";
        };
      });
}
