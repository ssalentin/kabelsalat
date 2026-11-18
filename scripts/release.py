#!/usr/bin/env python3
"""Changelog fragments and version bumps (docs/PACKAGING.md, "Releases").

Each PR adds one file under changes/ (any name ending in .md) holding Keep a
Changelog sections, e.g.

    ### Fixed
    - **Short title (#12):** what changed for the user.

so parallel PRs never edit the same CHANGELOG lines. A release folds the
fragments and whatever is still under "## [Unreleased]" into one version block.

usage:
  scripts/release.py check            fail on a malformed fragment
  scripts/release.py notes            print the unreleased notes
  scripts/release.py next <bump>      print the version a bump leads to
  scripts/release.py prepare <bump>   bump Cargo.toml/.lock, metainfo and
                                      CHANGELOG; delete the fragments
<bump> is patch, minor, major or an explicit X.Y.Z.
"""
import datetime
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CHANGES = ROOT / "changes"
CHANGELOG = ROOT / "CHANGELOG.md"
CARGO_TOML = ROOT / "Cargo.toml"
CARGO_LOCK = ROOT / "Cargo.lock"
METAINFO = ROOT / "packaging/flatpak/io.github.ssalentin.kabelsalat.metainfo.xml"
REPO = "https://github.com/ssalentin/kabelsalat"
SECTIONS = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]


def die(msg):
    print(f"release.py: {msg}", file=sys.stderr)
    sys.exit(1)


def parse_sections(text, where):
    """'### Name' headings -> {name: [lines]}; text before the first heading is an error."""
    out, cur = {}, None
    for n, line in enumerate(text.splitlines(), 1):
        m = re.match(r"^###\s+(.+?)\s*$", line)
        if m:
            cur = m.group(1)
            if cur not in SECTIONS:
                die(f"{where}:{n}: unknown section '### {cur}' (use one of {', '.join(SECTIONS)})")
            out.setdefault(cur, [])
        elif cur is None:
            if line.strip():
                die(f"{where}:{n}: text before the first '### <Section>' heading")
        else:
            out[cur].append(line)
    for name, lines in out.items():
        if not any(l.strip() for l in lines):
            die(f"{where}: section '### {name}' is empty")
    return out


def fragments():
    return sorted(p for p in CHANGES.glob("*.md") if p.name != "README.md")


def changelog_parts():
    text = CHANGELOG.read_text()
    m = re.search(r"^## \[Unreleased\]\n(.*?)(?=^## \[)", text, re.S | re.M)
    if not m:
        die("CHANGELOG.md has no '## [Unreleased]' block followed by a version")
    return text, m


def unreleased_notes():
    """Unreleased block plus every fragment, merged section by section."""
    _, m = changelog_parts()
    merged = {s: [] for s in SECTIONS}
    blocks = [("CHANGELOG.md [Unreleased]", m.group(1))]
    blocks += [(str(p.relative_to(ROOT)), p.read_text()) for p in fragments()]
    for where, text in blocks:
        for name, lines in parse_sections(text, where).items():
            while lines and not lines[-1].strip():
                lines.pop()
            while lines and not lines[0].strip():
                lines.pop(0)
            merged[name].extend(lines)
    out = []
    for name in SECTIONS:
        if merged[name]:
            out += [f"### {name}"] + merged[name] + [""]
    return "\n".join(out).strip() + "\n" if out else ""


def current_version():
    m = re.search(r'^version = "([^"]+)"', CARGO_TOML.read_text(), re.M)
    return m.group(1)


def next_version(bump):
    if re.fullmatch(r"\d+\.\d+\.\d+", bump):
        return bump
    major, minor, patch = map(int, current_version().split("."))
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    die(f"bump must be patch, minor, major or X.Y.Z, not '{bump}'")


def sub_once(pattern, repl, text, what):
    new, n = re.subn(pattern, repl, text, count=1, flags=re.M)
    if n != 1:
        die(f"could not update {what}")
    return new


def prepare(bump):
    old, new = current_version(), next_version(bump)
    notes = unreleased_notes()
    if not notes:
        die("nothing to release: no fragments in changes/ and an empty [Unreleased] block")
    date = datetime.date.today().isoformat()

    CARGO_TOML.write_text(sub_once(r'^version = "[^"]+"', f'version = "{new}"',
                                   CARGO_TOML.read_text(), "Cargo.toml version"))
    CARGO_LOCK.write_text(sub_once(r'^(name = "kabelsalat"\nversion = )"[^"]+"', rf'\1"{new}"',
                                   CARGO_LOCK.read_text(), "Cargo.lock kabelsalat entry"))
    entry = (f'<releases>\n    <release version="{new}" date="{date}">\n'
             f'      <url type="details">{REPO}/releases/tag/v{new}</url>\n    </release>')
    METAINFO.write_text(sub_once(r"<releases>", entry, METAINFO.read_text(), "metainfo <releases>"))

    text, m = changelog_parts()
    text = text[: m.start()] + f"## [Unreleased]\n\n## [{new}] — {date}\n\n{notes}\n" + text[m.end():]
    text = sub_once(r"^\[Unreleased\]: .*$",
                    f"[Unreleased]: {REPO}/compare/v{new}...HEAD\n"
                    f"[{new}]: {REPO}/compare/v{old}...v{new}", text, "CHANGELOG compare links")
    CHANGELOG.write_text(text)
    for p in fragments():
        p.unlink()
    print(new)


def main(argv):
    if len(argv) < 2:
        die(__doc__)
    cmd = argv[1]
    if cmd == "check":
        unreleased_notes()
        print(f"changelog ok: {len(fragments())} fragment(s)")
    elif cmd == "notes":
        sys.stdout.write(unreleased_notes())
    elif cmd == "next" and len(argv) == 3:
        print(next_version(argv[2]))
    elif cmd == "prepare" and len(argv) == 3:
        prepare(argv[2])
    else:
        die(__doc__)


if __name__ == "__main__":
    main(sys.argv)
