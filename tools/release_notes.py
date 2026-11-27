"""Notes of a GitHub release: the version's section of CHANGELOG.md, then the Unity package's.

    python tools/release_notes.py 0.13.0 > notes.md

The release workflow runs it before it builds anything, so a tag without a changelog section
publishes nothing. Every released section starts with **In short:** and **Upgrade:** lines
(docs/releases.md); `server/tests/test_release_notes.py` keeps the changelog that way.
"""
import argparse
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
SERVER_CHANGELOG = "CHANGELOG.md"
UNITY_CHANGELOG = "unity/com.yevhenii.purser/CHANGELOG.md"


def section(text, version):
    """The body of `## <version>` (or `## [<version>]`), up to the next `## ` heading; None
    when the changelog has no such section or it is empty."""
    heading = re.compile(r"^## \[?" + re.escape(version) + r"\]?(\s|$)")
    body = None
    for line in text.splitlines():
        if line.startswith("## "):
            if body is not None:
                break
            if heading.match(line):
                body = []
            continue
        if body is not None:
            body.append(line)
    if body is None:
        return None
    return "\n".join(body).strip() or None


def released_versions(text):
    """Versions of the released sections, newest first (`Unreleased` is not one)."""
    return re.findall(r"^## \[?(\d+\.\d+\.\d+)\]?", text, flags=re.M)


def notes(version, root=ROOT):
    with open(os.path.join(root, SERVER_CHANGELOG), encoding="utf-8") as f:
        server = section(f.read(), version)
    if server is None:
        raise LookupError(f"{SERVER_CHANGELOG} has no section for {version}")
    parts = [server]
    unity_path = os.path.join(root, UNITY_CHANGELOG)
    if os.path.exists(unity_path):
        with open(unity_path, encoding="utf-8") as f:
            unity = section(f.read(), version)
        if unity:
            parts.append("## Unity package\n\n" + unity)
    return "\n\n".join(parts) + "\n"


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("version", help="the version without the v of the tag, e.g. 0.13.0")
    p.add_argument("--root", default=ROOT, help="the repository root")
    args = p.parse_args(argv)
    try:
        text = notes(args.version, args.root)
    except LookupError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    # UTF-8 whatever the console's code page (the changelog has arrows and other non-ASCII text).
    sys.stdout.buffer.write(text.encode("utf-8"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
