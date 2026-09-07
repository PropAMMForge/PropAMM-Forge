#!/usr/bin/env bash
# Build of the on-chain part in WSL from a repository that lives on a Windows drive.
#
# Invoke from PowerShell, not from Git Bash:
#   wsl -e bash /mnt/<drive>/<path to repo>/scripts/wsl-build.sh build
#
# Three things here are not cosmetic:
#
# 1. Git Bash rewrites an argument of the form /mnt/<drive>/... into a Windows path
#    before wsl ever sees it. That is why the call comes from PowerShell.
# 2. The script is passed as a FILE, not as a string via `bash -c`: quotes and
#    dollars in a string pass through two layers of interpretation and silently change meaning.
# 3. PATH is set here explicitly. A non-interactive shell does not read ~/.profile,
#    so cargo-build-sbf and anchor are simply not found otherwise.

set -euo pipefail

export PATH="$HOME/.avm/bin:$HOME/.cargo/bin:$HOME/.local/share/solana/install/active_release/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$ROOT/.build.log"
CMD="${1:-build}"

cd "$ROOT"

# All output goes to the file FROM INSIDE the script.
#
# If redirected from outside, cargo's progress bar overwrites the line with a
# carriage return, and no cause of failure remains in the file — only the last
# progress frame. Then the file is read and its tail is shown.
run() {
  echo "── $* ──" >>"$LOG"
  if ! "$@" >>"$LOG" 2>&1; then
    echo "ERROR: $*"
    echo "── last 40 lines of $LOG ──"
    tail -40 "$LOG"
    exit 1
  fi
}

: >"$LOG"

echo "anchor:  $(anchor --version)"
echo "solana:  $(solana --version)"
echo "sbf:     $(cargo-build-sbf --version | head -1)"
echo "log:     $LOG"
echo

case "$CMD" in
  build)
    run anchor build
    echo "OK — artifacts in target/deploy, IDL in target/idl"
    ;;
  fmt)
    run cargo fmt --all
    echo "OK — formatted"
    ;;
  fmt-check)
    run cargo fmt --all --check
    echo "OK — format is clean"
    ;;
  test)
    # The instruction tests (T020) execute the .so itself, not the host build of the crate.
    # Without the artifact they fail — their message is clear, but saying it here
    # is cheaper than after a three-minute compilation.
    if [[ ! -f target/deploy/propamm_vault.so ]]; then
      echo "no target/deploy/propamm_vault.so — first: $0 build" >&2
      exit 1
    fi
    run cargo test --workspace
    echo "OK — tests passed"
    ;;
  clippy)
    run cargo clippy --workspace --all-targets -- -D warnings
    echo "OK — clippy is clean"
    ;;
  bench-cu)
    if [[ ! -f target/deploy/propamm_vault.so ]]; then
      echo "no target/deploy/propamm_vault.so — first: $0 build" >&2
      exit 1
    fi
    run cargo bench -p propamm-vault-tests
    # This table is what the benchmark is invoked for. It is printed last, so it is
    # taken from its header to the end of the log; without this line "OK" would be
    # the only thing visible on screen, and the numbers would stay in the file.
    sed -n '/Instruction CU/,$p' "$LOG"
    echo "OK — CU benchmark taken, SC-002 budget held"
    ;;
  golden)
    # Overwrites packages/sdk/tests/fixtures/borsh-golden.json with the bytes borsh
    # writes. Invoke only when the layout changed deliberately: without this variable
    # the same test only compares and has to be red on a mismatch.
    run env UPDATE_GOLDEN=1 cargo test -p propamm-vault --test golden_vectors
    echo "OK — golden vectors updated"
    ;;
  *)
    echo "unknown command: $CMD (build | fmt | fmt-check | test | clippy | bench-cu | golden)" >&2
    exit 2
    ;;
esac
