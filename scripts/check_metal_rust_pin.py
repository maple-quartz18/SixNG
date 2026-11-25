#!/usr/bin/env python3

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


METAL_RUST_PACKAGES = ("metal-rust", "metal-rust-ffi")
GIT_SOURCE_RE = re.compile(
    r"git\+https://github\.com/maple-quartz18/Metal-Rust\.git"
    r"\?rev=([0-9a-fA-F]+)#([0-9a-fA-F]+)"
)


class PinMismatchError(Exception):
    pass


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError as error:
        raise PinMismatchError(f"missing file: {path}") from error
    except UnicodeDecodeError as error:
        raise PinMismatchError(f"invalid UTF-8 in {path}: {error}") from error


def manifest_pins(text: str) -> dict[str, str]:
    match = re.search(
        r"(?ms)^\[patch\.crates-io\]\s*$\n(.*?)(?=^\[|\Z)",
        text,
    )
    if not match:
        raise PinMismatchError("Cargo.toml: missing [patch.crates-io]")
    patch_text = match.group(1)

    pins: dict[str, str] = {}
    for package in METAL_RUST_PACKAGES:
        entry_match = re.search(
            rf"(?m)^\s*{re.escape(package)}\s*=\s*\{{[^}}]*\}}",
            patch_text,
        )
        if not entry_match:
            raise PinMismatchError(
                f"Cargo.toml: missing patch.crates-io.{package}"
            )
        entry = entry_match.group(0)
        url_match = re.search(r'git\s*=\s*"([^"]+)"', entry)
        rev_match = re.search(r'rev\s*=\s*"([^"]+)"', entry)
        url = url_match.group(1) if url_match else None
        rev = rev_match.group(1) if rev_match else None
        if not url or "maple-quartz18/Metal-Rust" not in url:
            raise PinMismatchError(
                f"Cargo.toml: patch.crates-io.{package}.git is not the "
                "maple-quartz18/Metal-Rust fork"
            )
        if not rev or not re.fullmatch(r"[0-9a-fA-F]{7,64}", rev):
            raise PinMismatchError(
                f"Cargo.toml: patch.crates-io.{package}.rev must be an exact Git commit"
            )
        pins[package] = rev.lower()

    if len(set(pins.values())) != 1:
        raise PinMismatchError(
            "Cargo.toml: metal-rust and metal-rust-ffi pin different revisions "
            f"({pins['metal-rust']} vs {pins['metal-rust-ffi']})"
        )
    return pins


def lockfile_pins(text: str) -> dict[str, str]:
    pins: dict[str, str] = {}
    for package_text in re.findall(
        r"(?ms)^\[\[package\]\]\n(.*?)(?=^\[\[package\]\]|\Z)",
        text,
    ):
        name_match = re.search(r'(?m)^name\s*=\s*"([^"]+)"', package_text)
        if not name_match:
            continue
        name = name_match.group(1)
        if name not in METAL_RUST_PACKAGES:
            continue
        source_match = re.search(r'(?m)^source\s*=\s*"([^"]+)"', package_text)
        if not source_match:
            raise PinMismatchError(
                f"Cargo.lock: package {name} has no Git source"
            )
        source = source_match.group(1)
        match = GIT_SOURCE_RE.fullmatch(source)
        if not match:
            raise PinMismatchError(
                f"Cargo.lock: package {name} has unexpected Git source: {source}"
            )
        rev_from_query, rev_from_fragment = match.groups()
        if rev_from_query.lower() != rev_from_fragment.lower():
            raise PinMismatchError(
                f"Cargo.lock: package {name} has inconsistent source rev"
            )
        pins[name] = rev_from_query.lower()

    missing = [name for name in METAL_RUST_PACKAGES if name not in pins]
    if missing:
        raise PinMismatchError(
            f"Cargo.lock: missing package entries: {', '.join(missing)}"
        )
    return pins


def check_pins(manifest_path: Path, lockfile_path: Path) -> str:
    manifest_values = manifest_pins(read_text(manifest_path))
    lock_pins = lockfile_pins(read_text(lockfile_path))

    expected = next(iter(manifest_values.values()))
    mismatches: list[str] = []
    for package in METAL_RUST_PACKAGES:
        actual = lock_pins[package]
        if actual != expected:
            mismatches.append(
                f"{package}: manifest {expected}, lockfile {actual}"
            )
    if mismatches:
        raise PinMismatchError(
            "Cargo.lock drift detected: " + "; ".join(mismatches)
        )
    return expected


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Verify SixNG Metal-Rust manifest and lockfile pins"
    )
    parser.add_argument("--manifest", type=Path, default=Path("Cargo.toml"))
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    args = parser.parse_args(argv)

    try:
        rev = check_pins(args.manifest, args.lockfile)
    except PinMismatchError as error:
        print(f"metal-rust pin check failed: {error}", file=sys.stderr)
        return 1

    print(
        "metal-rust pin check passed: "
        f"{rev} for metal-rust and metal-rust-ffi"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
