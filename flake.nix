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
        runtimeTools = lib.optionals stdenv.isLinux
          (with pkgs; [ ydotool wl-clipboard xdg-utils grim wlr-randr ]);

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
          packages = [ pkgs.rustc pkgs.cargo pkgs.pkg-config pkgs.cmake ] ++ runtimeTools;
        };
      });
}
