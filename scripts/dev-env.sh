# The environment a developer's builds run in. Source it from the worktree's root, with the
# worktree's own CARGO_TARGET_DIR, before each cargo command:
#
#   . scripts/dev-env.sh && cargo nextest run -p reims-vgpu-core
#
# It sets CARGO_INCREMENTAL=0, as `cargo xtask check` builds. Where kache is on the PATH, rustc runs
# through it (RUSTC_WRAPPER=kache): a compiler cache that stores each unit's outputs on this machine,
# keyed by the unit's inputs, and restores them as clones of the stored files, so a worktree builds
# only the units no tree on the machine built before. kache stays local, whatever its configuration
# says (KACHE_LOCAL_ONLY=1), and keeps no incremental state in the target directory
# (KACHE_ADAPTIVE_INCREMENTAL=0). It runs each build script with its OUT_DIR in a directory of its
# store named by the script's inputs, which the target directory's OUT_DIR links to
# (KACHE_BUILD_SCRIPT_HERMETIC=1), so a script's run is shared between worktrees even where what it
# writes names its OUT_DIR, as SDL3's library does. Without kache the build is cargo's own: nothing
# in the repository needs kache, and it is never Cargo's configured wrapper (docs/development.md,
# "Building through kache").
export CARGO_INCREMENTAL=0
if command -v kache > /dev/null 2>&1; then
  export RUSTC_WRAPPER=kache KACHE_LOCAL_ONLY=1 KACHE_ADAPTIVE_INCREMENTAL=0 \
    KACHE_BUILD_SCRIPT_HERMETIC=1
fi
