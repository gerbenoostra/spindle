# Changelog

## [0.1.0](https://github.com/gerbenoostra/spindle/compare/v0.0.2...v0.1.0) (2026-10-02)


### Features

* **dashboard:** attention end to end - journal, hook, arbitration, seen-state ([#14](https://github.com/gerbenoostra/spindle/issues/14)) ([b407e91](https://github.com/gerbenoostra/spindle/commit/b407e91dfc33c1e7c5b0f38a824a8014873a959d))
* **dashboard:** Claude conversations on screen - snapshot, JSON, TUI shell ([#9](https://github.com/gerbenoostra/spindle/issues/9)) ([d63ed47](https://github.com/gerbenoostra/spindle/commit/d63ed472d07c2ff2734b3cfbbfd567582fbd50ad))
* **dashboard:** progressive collection - staged snapshots, scoped fan-out, batched Git ([#12](https://github.com/gerbenoostra/spindle/issues/12)) ([fdd6162](https://github.com/gerbenoostra/spindle/commit/fdd61620cd914e2d8ce87f916c6d7079c781b007))
* **git:** read-only Git substrate, shared state vector and cleanup verdicts ([#5](https://github.com/gerbenoostra/spindle/issues/5)) ([eb70f1f](https://github.com/gerbenoostra/spindle/commit/eb70f1feb2b7e83dab494c6c858e1a2bff6bd467))
* **runtime:** pane inventory, process-instance liveness and pid-to-pane resolution ([#8](https://github.com/gerbenoostra/spindle/issues/8)) ([ecd4ba5](https://github.com/gerbenoostra/spindle/commit/ecd4ba530322a82c9f2e724e2581859bc1a485a6))


### Bug Fixes

* keep the pre-push CI snapshot off the pushing worktree ([#13](https://github.com/gerbenoostra/spindle/issues/13)) ([dbbb876](https://github.com/gerbenoostra/spindle/commit/dbbb8769cafb671f6f2f86e48ffaef539c8403f4))

## [0.0.2](https://github.com/gerbenoostra/spindle/compare/v0.0.1...v0.0.2) (2026-09-24)


### Bug Fixes

* **config:** reject multi-byte duration units without panicking ([1fda29e](https://github.com/gerbenoostra/spindle/commit/1fda29e5958e676954be40b525990100b2e5fad0))
* **config:** require an absolute HOME for the fallback paths too ([96c8c7c](https://github.com/gerbenoostra/spindle/commit/96c8c7c91e3b646dc2769a0fde3e4d0156d34b98))
* **config:** treat empty or relative XDG variables as unset ([3712661](https://github.com/gerbenoostra/spindle/commit/37126612d8383197f5455b4183fbb54a0e1af3c1))
