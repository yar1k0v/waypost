"""Semver release logic, driven by Conventional Commits. Stdlib only.

    release.py next          print the next version, or nothing if no release is due
    release.py bump X.Y.Z    write X.Y.Z into Cargo.toml, crate path deps and Cargo.lock

Base version: the latest `vX.Y.Z` tag reachable from HEAD. With no tag, the
workspace version in Cargo.toml and every commit count.

Bump rules (version 0.x, Cargo style; from 1.0.0 the usual semver shift):
    breaking (`type!:` or `BREAKING CHANGE:` footer)  0.x: minor   1.x+: major
    feat                                              0.x: patch   1.x+: minor
    fix, perf                                         patch
    anything else (docs, build, ci, chore, ...)       no release
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SUBJECT = re.compile(r"^(?P<type>\w+)(?:\([^)]*\))?(?P<bang>!)?:\s")
BREAKING_FOOTER = re.compile(r"^BREAKING[ -]CHANGE:", re.MULTILINE)
TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
LEVELS = {"none": 0, "patch": 1, "minor": 2, "major": 3}


def level_of(message):
    """Release level one commit message asks for: none, patch, minor or major."""
    subject = message.splitlines()[0] if message.strip() else ""
    if subject.startswith("chore(release)"):
        return "none"
    m = SUBJECT.match(subject)
    if not m:
        return "none"
    if m["bang"] or BREAKING_FOOTER.search(message):
        return "major"
    if m["type"] == "feat":
        return "minor"
    if m["type"] in ("fix", "perf"):
        return "patch"
    return "none"


def next_version(base, messages):
    """Next (major, minor, patch) for base and the commit messages since it, or None."""
    top = max((LEVELS[level_of(m)] for m in messages), default=0)
    if top == 0:
        return None
    major, minor, patch = base
    if major == 0:
        top -= 1  # 0.x: breaking -> minor, feat and fix -> patch
        if top == 0:
            top = 1  # fix stays a patch
    if top == LEVELS["major"]:
        return (major + 1, 0, 0)
    if top == LEVELS["minor"]:
        return (major, minor + 1, 0)
    return (major, minor, patch + 1)


def git(*args):
    return subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout


def workspace_version():
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    section = text.split("[workspace.package]", 1)[1]
    return tuple(int(x) for x in re.search(r'^version = "(\d+)\.(\d+)\.(\d+)"', section, re.M).groups())


def base_and_range():
    described = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "HEAD"],
        cwd=ROOT, capture_output=True, text=True,
    )
    tag = described.stdout.strip()
    m = TAG.match(tag)
    if described.returncode == 0 and m:
        return tuple(int(x) for x in m.groups()), f"{tag}..HEAD"
    return workspace_version(), "HEAD"


def commit_messages(rev_range):
    out = git("log", "--format=%B%x1e", rev_range)
    return [m.strip() for m in out.split("\x1e") if m.strip()]


def bump(version):
    major, minor, patch = (int(x) for x in version.split("."))
    new = f"{major}.{minor}.{patch}"
    root_toml = ROOT / "Cargo.toml"
    text = root_toml.read_text(encoding="utf-8")
    head, sep, tail = text.partition("[workspace.package]")
    tail = re.sub(r'^version = "[^"]+"', f'version = "{new}"', tail, count=1, flags=re.M)
    root_toml.write_text(head + sep + tail, encoding="utf-8", newline="\n")
    for toml in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        text = toml.read_text(encoding="utf-8")
        updated = re.sub(
            r'^(waypost-[\w-]+ = \{ version = ")[^"]+(", path = )',
            rf"\g<1>{new}\g<2>",
            text,
            flags=re.M,
        )
        if updated != text:
            toml.write_text(updated, encoding="utf-8", newline="\n")
    subprocess.run(["cargo", "update", "--workspace"], cwd=ROOT, check=True)
    subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)


def main(argv):
    if len(argv) == 2 and argv[1] == "next":
        base, rev_range = base_and_range()
        nxt = next_version(base, commit_messages(rev_range))
        if nxt:
            print(".".join(str(x) for x in nxt))
        return 0
    if len(argv) == 3 and argv[1] == "bump" and re.fullmatch(r"\d+\.\d+\.\d+", argv[2]):
        bump(argv[2])
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
