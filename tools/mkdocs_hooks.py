"""MkDocs hook (see mkdocs.yml): links from docs/ to other files of the repository.

The Markdown files in docs/ link to files outside the docs folder, such as ../server/README.md,
so that the links work when the files are read on GitHub. On the docs site those files do not
exist; this hook rewrites such links to their GitHub URLs, so `mkdocs build --strict` passes and
the links keep working. Links inside docs/ are left to MkDocs (and still validated).
"""

import os
import re

BRANCH = "main"

# [text](target) or [text](target "title"); the target has no spaces.
LINK = re.compile(r"(\]\()([^)\s]+)((?:\s+\"[^\"]*\")?\))")
FENCE = re.compile(r"^\s*(```|~~~)")


def _rewrite(target, page_dir, docs_dir, repo_root, repo_url):
    if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I) or target.startswith(("#", "/")):
        return target
    path, sep, anchor = target.partition("#")
    if not path:
        return target
    abs_path = os.path.normpath(os.path.join(page_dir, path))
    if os.path.commonpath([abs_path, docs_dir]) == docs_dir:
        return target
    if os.path.commonpath([abs_path, repo_root]) != repo_root:
        return target
    rel = os.path.relpath(abs_path, repo_root).replace(os.sep, "/")
    kind = "tree" if os.path.isdir(abs_path) else "blob"
    return f"{repo_url}/{kind}/{BRANCH}/{rel}{sep}{anchor}"


def on_page_markdown(markdown, page, config, files):
    repo_url = (config.get("repo_url") or "").rstrip("/")
    if not repo_url:
        return markdown
    docs_dir = os.path.normpath(os.path.abspath(config["docs_dir"]))
    repo_root = os.path.normpath(os.path.dirname(os.path.abspath(config["config_file_path"])))
    page_dir = os.path.dirname(os.path.abspath(page.file.abs_src_path))

    out, in_fence = [], False
    for line in markdown.splitlines(keepends=True):
        if FENCE.match(line):
            in_fence = not in_fence
        if not in_fence:
            line = LINK.sub(
                lambda m: m.group(1) + _rewrite(m.group(2), page_dir, docs_dir, repo_root, repo_url) + m.group(3),
                line,
            )
        out.append(line)
    return "".join(out)
