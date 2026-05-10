#!/usr/bin/env bash
# Run a cargo command for the firmware crate inside the espressif/idf-rust container.
# This sidesteps host toolchain issues (e.g. RHEL 9's libstdc++ being too old for
# the espup-installed Rust). Caches are persisted on the host so re-runs are fast.
#
# Usage:
#   scripts/firmware.sh build           # cargo build
#   scripts/firmware.sh build --release
#   scripts/firmware.sh check
#   scripts/firmware.sh clippy
#   scripts/firmware.sh shell           # interactive bash inside the container
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${WC_IMAGE:-docker.io/espressif/idf-rust:esp32_latest}"

# Persistent caches on the host (created on first run). We mount only the
# cache subdirs of ~/.cargo so we don't clobber the cargo binary that lives
# at ~/.cargo/bin/cargo inside the container.
CARGO_REGISTRY="${WC_CARGO_REGISTRY:-$HOME/.cache/watercontroller-cargo/registry}"
CARGO_GIT="${WC_CARGO_GIT:-$HOME/.cache/watercontroller-cargo/git}"
ESPRESSIF_HOME="${WC_ESPRESSIF_CACHE:-$HOME/.cache/watercontroller-espressif}"
mkdir -p "$CARGO_REGISTRY" "$CARGO_GIT" "$ESPRESSIF_HOME"

# Map the host user's uid to uid 1000 (the container's `esp` user) so that
# bind-mounted host directories are owned by the container's `esp` user from
# the container's perspective, while still being owned by the real host user
# from outside.
PODMAN_ARGS=(
    --rm
    --userns=keep-id:uid=1000,gid=1000
    -v "$REPO_ROOT":/project:Z
    -v "$CARGO_REGISTRY":/home/esp/.cargo/registry:Z
    -v "$CARGO_GIT":/home/esp/.cargo/git:Z
    -v "$ESPRESSIF_HOME":/home/esp/.espressif:Z
    -w /project/crates/firmware
    -e CARGO_TARGET_DIR=/project/target/firmware
)

if [[ "${1:-}" == "shell" ]]; then
    exec podman run -it "${PODMAN_ARGS[@]}" "$IMAGE" bash -l
fi

# Pass remaining args to `cargo`.
exec podman run "${PODMAN_ARGS[@]}" "$IMAGE" cargo "$@"
