#!/usr/bin/env bash
# Linux counterpart of check-baseline.ps1. Requires Python 3, the repository's
# Rust toolchain, and dependencies already fetched for both Cargo workspaces.
set -euo pipefail

if ! command -v python3 >/dev/null 2>&1; then
    printf '%s\n' 'Python 3 is required to run the baseline checks.' >&2
    exit 1
fi

exec python3 - "${BASH_SOURCE[0]}" "$@" <<'PY'
import argparse
import datetime
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


def job_count(value):
    try:
        count = int(value)
    except ValueError:
        raise argparse.ArgumentTypeError("jobs must be an integer from 1 to 32")
    if not 1 <= count <= 32:
        raise argparse.ArgumentTypeError("jobs must be an integer from 1 to 32")
    return count


parser = argparse.ArgumentParser(
    prog="check-baseline.sh",
    description="Run all nine offline baseline checks and save logs and summary.json.",
)
parser.add_argument("--jobs", type=job_count, default=4, help="Cargo build jobs, 1..32 (default: 4)")
options = parser.parse_args(sys.argv[2:])
repository_root = Path(sys.argv[1]).resolve().parent.parent

# Resolve Cargo through PATH, with the conventional rustup location as fallback.
cargo = shutil.which("cargo")
if cargo is None:
    candidate = Path.home() / ".cargo" / "bin" / "cargo"
    if candidate.is_file() and os.access(candidate, os.X_OK):
        cargo = str(candidate)
if cargo is None:
    parser.exit(1, "Cargo is not installed or cannot be found.\n")
# PATH entries can be relative to the caller's directory; preserve the resolved
# location when subprocesses change to the repository root, without following
# the cargo -> rustup symlink (rustup dispatches by the executable name).
cargo = os.path.abspath(cargo)
for lockfile, message in [
    ("Cargo.lock", "Run cargo fetch in the repository first to resolve and download the baseline dependencies."),
    ("tests/model-protocol/Cargo.lock", "Restore/prepare the private model-protocol validation workspace and lockfile first."),
]:
    if not (repository_root / lockfile).is_file():
        parser.exit(1, message + "\n")

# Keep environment changes local to child processes. Cargo runs from the root,
# so rustup selects rust-toolchain.toml instead of a caller's toolchain override.
environment = os.environ.copy()
environment.pop("RUSTUP_TOOLCHAIN", None)
environment["PATH"] = str(Path(cargo).parent) + os.pathsep + environment.get("PATH", "")
baseline_directory = repository_root / "results" / "baseline"
baseline_directory.mkdir(parents=True, exist_ok=True)
timestamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
run_directory = Path(tempfile.mkdtemp(prefix=timestamp + "-", dir=baseline_directory))
jobs = str(options.jobs)
checks = [
    ("fmt", ["fmt", "--all", "--", "--check"]),
    ("check", ["check", "--workspace", "--all-targets", "--all-features", "--locked", "--offline", "--jobs", jobs]),
    ("default-check", ["check", "--package", "jingwei", "--no-default-features", "--locked", "--offline", "--jobs", jobs]),
    ("test", ["test", "--workspace", "--all-features", "--locked", "--offline", "--jobs", jobs]),
    ("clippy", ["clippy", "--workspace", "--all-targets", "--all-features", "--locked", "--offline", "--jobs", jobs, "--", "-D", "warnings"]),
    ("doc", ["doc", "--workspace", "--all-features", "--no-deps", "--locked", "--offline", "--jobs", jobs]),
    ("private-fmt", ["fmt", "--manifest-path", "tests/model-protocol/Cargo.toml", "--", "--check"]),
    ("private-test", ["test", "--manifest-path", "tests/model-protocol/Cargo.toml", "--locked", "--offline", "--target-dir", "target", "--jobs", jobs]),
    ("private-clippy", ["clippy", "--manifest-path", "tests/model-protocol/Cargo.toml", "--all-targets", "--locked", "--offline", "--target-dir", "target", "--jobs", jobs, "--", "-D", "warnings"]),
]
results = []

print("Baseline logs: {}".format(run_directory), flush=True)
for name, arguments in checks:
    stdout_path = run_directory / (name + ".stdout.log")
    stderr_path = run_directory / (name + ".stderr.log")
    command = " ".join(shlex.quote(argument) for argument in ["cargo"] + arguments)
    print("Starting: " + command, flush=True)
    started_at = time.monotonic()
    with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
        try:
            process = subprocess.run(
                [cargo] + arguments,
                cwd=repository_root,
                env=environment,
                stdout=stdout,
                stderr=stderr,
                check=False,
            )
            exit_code = process.returncode
        except OSError as error:
            stderr.write("Could not start Cargo: {}\n".format(error))
            exit_code = 127
    results.append({
        "Name": name,
        "Command": command,
        "ExitCode": exit_code,
        "ElapsedSeconds": round(time.monotonic() - started_at, 2),
        "Stdout": str(stdout_path),
        "Stderr": str(stderr_path),
    })
    # Preserve completed results even if a later check is interrupted.
    (run_directory / "summary.json").write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
    print("Finished {}: exit={}".format(name, exit_code), flush=True)

print("\n{:<18} {:>8} {:>16}".format("Name", "ExitCode", "ElapsedSeconds"))
for result in results:
    print("{Name:<18} {ExitCode:>8} {ElapsedSeconds:>16.2f}".format(**result))
sys.exit(1 if any(result["ExitCode"] != 0 for result in results) else 0)
PY
