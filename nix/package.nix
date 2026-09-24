{
  lib,
  rustPlatform,
  git,
  tmux,
  unixtools,
}:

rustPlatform.buildRustPackage {
  pname = "agent-sessions";
  version = (lib.importTOML ../Cargo.toml).package.version;

  # `.tasks` is an ignored symlink into a private task store: excluded here so
  # the packaged source never carries it, wherever it points on a dev machine.
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type: !(builtins.elem (baseNameOf path) [ ".tasks" ".git" ]);
  };

  # The lock file rather than a vendor hash: this is a binary crate whose
  # Cargo.lock is committed, so there is nothing to regenerate on a bump.
  cargoLock.lockFile = ../Cargo.lock;

  # The suite drives throwaway tmux servers and disposable git repositories,
  # which need a real, writable $HOME: the Linux sandbox's placeholder exists
  # (so `cd` into it works, if uselessly), but the Darwin sandbox never creates
  # it at all, and spawning against a missing cwd fails outright.
  #
  # `unixtools` picks the right `ps`/`hostname` implementation per platform
  # (the real ones on Darwin, `procps`/`inetutils` on Linux); the build
  # sandbox's minimal $PATH carries neither.
  nativeCheckInputs = [
    git
    tmux
    unixtools.hostname
    unixtools.ps
  ];
  preCheck = ''
    export HOME=$(mktemp -d)
  '';

  postInstall = ''
    if [ -d share ]; then
      cp -RL share "$out/"
    fi
  '';

  meta = {
    description = "One dashboard for every agentic session and worktree";
    homepage = "https://github.com/gerbenoostra/spindle";
    license = lib.licenses.mit;
    mainProgram = "agent-sessions";
    platforms = lib.platforms.unix;
  };
}
