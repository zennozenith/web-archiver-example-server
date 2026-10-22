{
  pkgs,
  lib,
  ...
}: {
  languages.rust = {
    enable = true;
    channel = "nightly";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rust-analyzer"
    ];

    mold.enable = true;
  };

  packages = with pkgs; [
    bacon
    cargo-nextest
    cargo-update
    cargo-machete
    cargo-udeps

    openssl
    pkg-config

    watchexec
    ## TODO: Vite+ external installation required
  ];

  tasks."app:test".exec = "cargo test";

  tasks."init:frontend" = {
    exec = "vp install";
    cwd = "./frontend";
  };

  tasks."frontend:build" = {
    exec = "vpr lint && vpr fmt && vpr build";
    execIfModified = [
      "frontend"
    ];
    cwd = "./frontend";
  };

  # tasks."frontend:tailwind" = {
  #   exec = ''
  #     echo $PWD
  #     bunx @tailwindcss/cli -i ./frontend/input.css -o ./static/css/build/tailwind.css
  #   '';
  #   execIfModified = [
  #     "frontend/input.css"
  #     "templates"
  #   ];
  #   before = ["frontend:build"];
  # };

  scripts.watcher = {
    exec = ''
      watchexec -q -c -w src/ -w templates/ -w frontend/ -r --stop-signal SIGKILL "cargo run"
    '';
    packages = [pkgs.watchexec];
  };

  scripts.watch-tailwind = {
    exec = ''
      watchexec -e html,css -w frontend/input.css -w templates/ \
      -- devenv tasks run frontend:tailwind
    '';
    packages = [
      pkgs.tailwindcss_4
      pkgs.watchexec
    ];
  };

  env.LD_LIBRARY_PATH = lib.makeLibraryPath [
    pkgs.zlib
  ];

  env = {
    RUST_LOG = "info,myopic=debug";
    APP_SERVER__HOST = "0.0.0.0";
    APP_SERVER__PORT = 8888;
    APP_DATABASE__URL = "sqlite://db/app.db";
    APP_DATA_DIR = "./data";
  };

  # git-hooks.hooks = {
  #   rustfmt.enable = true;
  #   clippy.enable = true;
  # };

  enterShell = ''
    echo "Crates ready to update with 'cargo update -n'":
  '';
  # See full reference at https://devenv.sh/reference/options/
}
