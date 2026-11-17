"""MkDocs build hooks for the Pari documentation site."""

from __future__ import annotations

import posixpath
import re
from typing import Any

REPOSITORY_BLOB = "https://github.com/dipeshbabu/pari/blob/main/"
# Inline Markdown links and images whose target is a relative path.
RELATIVE_LINK = re.compile(r"(\]\()(?!https?:|mailto:|#|/)([^)\s]+)(\))")


def on_page_markdown(
    markdown: str, *, page: Any, config: Any, files: Any, **_: Any
) -> str:
    """Point links that leave ``docs/`` at the repository on GitHub.

    The Markdown sources are also read directly on GitHub, where links such as
    ``../CHANGELOG.md`` or ``../benchmarks/results/...`` resolve. The published
    site only contains ``docs/``, so those targets are rewritten to the
    repository's ``main`` branch instead of becoming broken links.
    """

    source_dir = posixpath.dirname(page.file.src_uri)

    def rewrite(match: re.Match[str]) -> str:
        target = match.group(2)
        path, _, fragment = target.partition("#")
        resolved = posixpath.normpath(posixpath.join("docs", source_dir, path))
        if resolved.startswith("docs/"):
            return match.group(0)
        url = REPOSITORY_BLOB + resolved
        if fragment:
            url += "#" + fragment
        return f"{match.group(1)}{url}{match.group(3)}"

    return RELATIVE_LINK.sub(rewrite, markdown)
