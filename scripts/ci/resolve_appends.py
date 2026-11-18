#!/usr/bin/env python3
"""Resolve merge conflicts in which both sides only ADDED lines at one spot.

Parallel PRs append to the same lists (AGENTS.md rules, the CHANGELOG
Unreleased block of older branches); git calls that a conflict although
keeping both additions is the answer. For each conflicted file in the current
merge, this rebuilds the file from the index stages with `git merge-file
--diff3` and, when every conflict hunk has an empty base (pure insertions on
both sides), keeps ours then theirs and stages the file. Any other conflict
is left untouched, and the script exits 1 naming it. Only Markdown files are
resolved: two insertions in code need not compile together, so a code
conflict is always left to the PR's author.

usage (inside a conflicted `git merge`): scripts/ci/resolve_appends.py
"""
import os
import subprocess
import sys
import tempfile


def git(*args, check=True):
    return subprocess.run(["git", *args], check=check, capture_output=True, text=True).stdout


def stage_blob(stage, path):
    r = subprocess.run(["git", "show", f":{stage}:{path}"], capture_output=True)
    return r.stdout if r.returncode == 0 else None


def resolve(path):
    """Merged text when every hunk is insert/insert, else None."""
    base, ours, theirs = (stage_blob(n, path) for n in (1, 2, 3))
    if ours is None or theirs is None:
        return None  # added/deleted on one side: not an append conflict
    with tempfile.TemporaryDirectory() as d:
        files = []
        for name, blob in (("ours", ours), ("base", base or b""), ("theirs", theirs)):
            p = os.path.join(d, name)
            with open(p, "wb") as f:
                f.write(blob)
            files.append(p)
        r = subprocess.run(["git", "merge-file", "-p", "--diff3", "-L", "ours", "-L", "base",
                            "-L", "theirs", *files], capture_output=True)
    text = r.stdout.decode()
    out, state, base_lines, theirs_lines, ours_lines = [], None, [], [], []
    for line in text.splitlines(keepends=True):
        if state is None and line.startswith("<<<<<<< ours"):
            state, ours_lines, base_lines, theirs_lines = "ours", [], [], []
        elif state == "ours" and line.startswith("||||||| base"):
            state = "base"
        elif state in ("ours", "base") and line.startswith("======="):
            state = "theirs"
        elif state == "theirs" and line.startswith(">>>>>>> theirs"):
            if any(l.strip() for l in base_lines):
                return None  # both sides changed existing lines
            out += ours_lines + theirs_lines
            state = None
        elif state == "ours":
            ours_lines.append(line)
        elif state == "base":
            base_lines.append(line)
        elif state == "theirs":
            theirs_lines.append(line)
        else:
            out.append(line)
    return None if state else "".join(out)


def main():
    conflicted = [p for p in git("diff", "--name-only", "--diff-filter=U").splitlines() if p]
    left = []
    for path in conflicted:
        merged = resolve(path) if path.endswith(".md") else None
        if merged is None:
            left.append(path)
            continue
        with open(path, "w") as f:
            f.write(merged)
        git("add", "--", path)
        print(f"resolved (both sides appended): {path}")
    if left:
        print("not an append-only conflict: " + " ".join(left), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
