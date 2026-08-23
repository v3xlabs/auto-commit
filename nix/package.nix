{
  lib,
  rustPlatform,
  installShellFiles,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "auto-commit";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [installShellFiles pkg-config];
  buildInputs = [openssl];

  # build.rs writes the completions and the man page into target/dist.
  postInstall = ''
    installManPage target/dist/auto-commit.1
    installShellCompletion --cmd auto-commit \
      --bash target/dist/auto-commit.bash \
      --fish target/dist/auto-commit.fish \
      --zsh target/dist/_auto-commit
  '';

  meta = {
    description = "Automagically generate commit messages";
    homepage = "https://github.com/v3xlabs/auto-commit";
    license = lib.licenses.mit;
    mainProgram = "auto-commit";
  };
}
