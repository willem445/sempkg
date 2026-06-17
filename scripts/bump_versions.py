#!/usr/bin/env python3
"""Bump version fields in TOML manifests.

This script updates:
- pyproject.toml -> [project].version
- Cargo.toml     -> [package].version

It scans the repository by default, but you can also pass explicit file paths.

Examples:
    python scripts/bump_versions.py 0.2.0
    python scripts/bump_versions.py 0.2.0 --dry-run
    python scripts/bump_versions.py 0.2.0 --root C:\\Projects\\codegraph-hub
    python scripts/bump_versions.py 0.2.0 --files pyproject.toml src/sempkg/Cargo.toml
"""

from __future__ import annotations

import argparse
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Iterator

SECTION_RE = re.compile(r"^\s*\[(?P<section>[^\]]+)\]\s*(?:#.*)?$")
VERSION_RE = re.compile(r'^(?P<prefix>\s*version\s*=\s*")[^"]*(?P<suffix>"\s*(?:#.*)?)$')

EXCLUDED_DIRS = {".git", "target", ".venv", "node_modules", "__pycache__"}


@dataclass
class FileChange:
    path: Path
    old_version: str
    new_version: str


def iter_candidate_files(root: Path, explicit_files: list[Path] | None = None) -> Iterator[Path]:
    if explicit_files:
        for file_path in explicit_files:
            yield file_path if file_path.is_absolute() else root / file_path
        return

    for pattern in ("pyproject.toml", "Cargo.toml"):
        for path in root.rglob(pattern):
            if any(part in EXCLUDED_DIRS for part in path.parts):
                continue
            yield path


def update_file(path: Path, new_version: str) -> FileChange | None:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)

    target_section = "project" if path.name == "pyproject.toml" else "package" if path.name == "Cargo.toml" else None
    if target_section is None:
        return None

    current_section = None
    updated = False
    old_version = None
    output_lines: list[str] = []

    for line in lines:
        section_match = SECTION_RE.match(line)
        if section_match:
            current_section = section_match.group("section").strip()
            output_lines.append(line)
            continue

        if current_section == target_section:
            version_match = VERSION_RE.match(line)
            if version_match:
                old_version = line.split('"', 2)[1]
                line = f'{version_match.group("prefix")}{new_version}{version_match.group("suffix")}\n' if not line.endswith("\n") else f'{version_match.group("prefix")}{new_version}{version_match.group("suffix")}\n'
                updated = True
        output_lines.append(line)

    if not updated:
        return None

    path.write_text("".join(output_lines), encoding="utf-8")
    return FileChange(path=path, old_version=old_version or "", new_version=new_version)


def main() -> int:
    parser = argparse.ArgumentParser(description="Bump version fields in TOML manifests.")
    parser.add_argument("version", help="New version string, e.g. 0.2.0")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.cwd(),
        help="Repository root to scan (default: current directory).",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would change without writing files.",
    )
    parser.add_argument(
        "--files",
        nargs="*",
        type=Path,
        help="Optional explicit manifest files to update instead of scanning.",
    )
    args = parser.parse_args()

    root = args.root.resolve()
    candidates = list(iter_candidate_files(root, args.files))
    if not candidates:
        print("No candidate TOML files found.")
        return 1

    changes: list[FileChange] = []
    skipped: list[Path] = []

    for path in candidates:
        if not path.exists():
            skipped.append(path)
            continue

        target_section = "project" if path.name == "pyproject.toml" else "package" if path.name == "Cargo.toml" else None
        if target_section is None:
            continue

        text = path.read_text(encoding="utf-8")
        lines = text.splitlines(keepends=True)
        current_section = None
        old_version = None
        found = False

        for line in lines:
            section_match = SECTION_RE.match(line)
            if section_match:
                current_section = section_match.group("section").strip()
                continue
            if current_section == target_section:
                version_match = VERSION_RE.match(line)
                if version_match:
                    old_version = line.split('"', 2)[1]
                    found = True
                    break

        if not found:
            skipped.append(path)
            continue

        changes.append(FileChange(path=path, old_version=old_version or "", new_version=args.version))

    if not changes:
        print("No version fields were updated.")
        if skipped:
            print("Skipped:")
            for path in skipped:
                print(f"  {path}")
        return 1

    if args.dry_run:
        for change in changes:
            rel = change.path.relative_to(root) if change.path.is_relative_to(root) else change.path
            print(f"{rel}: {change.old_version} -> {change.new_version}")
        return 0

    for change in changes:
        update_file(change.path, args.version)

    for change in changes:
        rel = change.path.relative_to(root) if change.path.is_relative_to(root) else change.path
        print(f"Updated {rel}: {change.old_version} -> {change.new_version}")

    if skipped:
        print("Skipped files without a matching version field:")
        for path in skipped:
            rel = path.relative_to(root) if path.is_relative_to(root) else path
            print(f"  {rel}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
