#!/usr/bin/env bash
# Checks run by ~/.claude/scripts/generate_state.sh (section CHECKS) instead of its defaults.
# Unit tests only: the full `cargo test --workspace` (integration builds, ~3+ min with ffmpeg)
# exceeds the script's 600 s cap; run it separately before releases.
set -e
CI=1 npm test --silent
cargo test --workspace --lib --bins
