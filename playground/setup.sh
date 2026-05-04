#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# setup.sh — one-command playground setup.
#
# What it does:
#   1. Locate a Python 3.12+ interpreter.
#   2. Create .venv/ in the playground directory (idempotent — won't
#      clobber an existing venv).
#   3. Install maturin into the venv.
#   4. Build the hindsight Python extension (`maturin develop`) from
#      crates/hindsight-py/ into the active venv.
#   5. Verify by importing hindsight.
#   6. Print next steps.
#
# Run from the playground directory:
#
#     bash setup.sh
#
# The script is idempotent: re-running it just rebuilds the extension.
# It works on macOS, Linux, and Windows (Git Bash / MSYS).

set -euo pipefail

# --- Locate this script's directory and the workspace root ------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PY_CRATE="${WORKSPACE_ROOT}/crates/hindsight-py"
VENV_DIR="${SCRIPT_DIR}/.venv"

cd "${SCRIPT_DIR}"

# --- Find a Python 3.12+ interpreter ----------------------------------------

# Search order: PYTHON env var, py launcher (Windows), python3.12, python3, python.
find_python() {
    local candidates=()
    if [[ -n "${PYTHON:-}" ]]; then
        candidates+=("${PYTHON}")
    fi
    candidates+=("python3.12" "python3" "python")
    # Windows-style py launcher.
    if command -v py >/dev/null 2>&1; then
        candidates+=("py -3.12")
    fi

    for cmd in "${candidates[@]}"; do
        # shellcheck disable=SC2086
        if version=$($cmd -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null); then
            major="${version%%.*}"
            minor="${version##*.}"
            if [[ "${major}" -gt 3 || ( "${major}" -eq 3 && "${minor}" -ge 12 ) ]]; then
                echo "${cmd}"
                return 0
            fi
        fi
    done
    return 1
}

if ! PYTHON_CMD="$(find_python)"; then
    echo "ERROR: Python 3.12 or newer not found." >&2
    echo "Hindsight requires sys.monitoring (PEP 669), available only in 3.12+." >&2
    echo "Set the PYTHON env var to the right interpreter and retry, e.g.:" >&2
    echo "  PYTHON=/path/to/python3.12 bash setup.sh" >&2
    exit 1
fi

echo "Using Python: ${PYTHON_CMD}"
${PYTHON_CMD} --version

# --- Create venv (idempotent) -----------------------------------------------

if [[ -d "${VENV_DIR}" ]]; then
    echo "Reusing existing venv at ${VENV_DIR}"
else
    echo "Creating venv at ${VENV_DIR}"
    ${PYTHON_CMD} -m venv "${VENV_DIR}"
fi

# --- Pick the right activate-script + venv-python depending on platform ----

if [[ -f "${VENV_DIR}/Scripts/python.exe" ]]; then
    # Windows (Git Bash / MSYS).
    VENV_PYTHON="${VENV_DIR}/Scripts/python.exe"
    ACTIVATE_HINT="source .venv/Scripts/activate"
elif [[ -f "${VENV_DIR}/bin/python" ]]; then
    # macOS / Linux.
    VENV_PYTHON="${VENV_DIR}/bin/python"
    ACTIVATE_HINT="source .venv/bin/activate"
else
    echo "ERROR: venv layout looks unfamiliar — no python under .venv/Scripts or .venv/bin." >&2
    exit 1
fi

# Export so child processes (maturin, pip) see the venv as active.
export VIRTUAL_ENV="${VENV_DIR}"
export PATH="$(dirname "${VENV_PYTHON}"):${PATH}"

echo "Upgrading pip..."
"${VENV_PYTHON}" -m pip install --upgrade pip >/dev/null

# --- Install maturin --------------------------------------------------------

if "${VENV_PYTHON}" -m pip show maturin >/dev/null 2>&1; then
    echo "maturin already installed in venv."
else
    echo "Installing maturin..."
    "${VENV_PYTHON}" -m pip install 'maturin>=1.4,<2.0'
fi

# Also install duckdb so users can run query scripts that need the
# Python bindings (the `duckdb` CLI is a separate install — see README).
if ! "${VENV_PYTHON}" -m pip show duckdb >/dev/null 2>&1; then
    echo "Installing duckdb python bindings..."
    "${VENV_PYTHON}" -m pip install duckdb
fi

# --- Build hindsight via maturin develop -----------------------------------

echo "Building hindsight extension via maturin..."
echo "  (this compiles the Rust core; first build is slow, subsequent ones are fast)"
(
    cd "${PY_CRATE}"
    "${VENV_PYTHON}" -m maturin develop --release
)

# --- Verify -----------------------------------------------------------------

echo "Verifying installation..."
"${VENV_PYTHON}" -c "import hindsight; print('hindsight package:', hindsight.__name__); print('exports:', hindsight.__all__)"

# --- Build the indexer CLI (cargo) so the user can `hindsight index` -------
#
# The CLI lives in the workspace and isn't installed via pip. We build it
# in the workspace target/ so `cargo run -p hindsight-cli` works fast.

if command -v cargo >/dev/null 2>&1; then
    echo "Building hindsight CLI (cargo)..."
    (cd "${WORKSPACE_ROOT}" && cargo build -p hindsight-cli --release)
    CLI_BIN="${WORKSPACE_ROOT}/target/release/hindsight"
    if [[ ! -f "${CLI_BIN}" && -f "${CLI_BIN}.exe" ]]; then
        CLI_BIN="${CLI_BIN}.exe"
    fi
    echo "CLI built at: ${CLI_BIN}"
else
    echo "WARNING: cargo not found; skipping CLI build. Install Rust from https://rustup.rs/ then re-run." >&2
    CLI_BIN="(cargo not found — install Rust)"
fi

# --- Next steps -------------------------------------------------------------

cat <<EOF

============================================================
Setup complete.
============================================================

To activate the venv in your shell:
    ${ACTIVATE_HINT}

Then, from the playground directory, try:

    # 1. Record a trace:
    python examples/basic.py

    # 2. Index it:
    "${CLI_BIN}" index trace.hindsight

    # 3. Query it:
    duckdb trace.duckdb     # or: python -c "import duckdb; ..."

See README.md for the full walkthrough.
EOF
