"""MkDocs build hooks for the Pari documentation site."""

from __future__ import annotations

import posixpath
import re
from typing import Any

REPOSITORY_BLOB = "https://github.com/dipeshbabu/pari/blob/main/"
# Inline Markdown links and images whose target is a relative path.
RELATIVE_LINK = re.compile(r"(\]\()(?!https?:|mailto:|#|/)([^)\s]+)(\))")

# Navigation icons, assigned here so the Markdown sources need no front matter
# (which GitHub would otherwise render as a table above each guide).
PAGE_ICONS = {
    "index.md": "material/home",
    "python.md": "material/language-python",
    "cli.md": "material/console",
    "dataset-integrations.md": "material/database-import",
    "text-workloads.md": "material/text-box-search-outline",
    "code-workloads.md": "material/code-braces",
    "entity-matching.md": "material/account-search",
    "architecture.md": "material/sitemap",
    "minhash.md": "material/fingerprint",
    "lsh.md": "material/pound",
    "grouping.md": "material/group",
    "planning.md": "material/chart-bell-curve",
    "parallelism.md": "material/cpu-64-bit",
    "observability.md": "material/chart-line",
    "persistence.md": "material/content-save",
    "storage-backends.md": "material/server",
    "index-format.md": "material/file-cog-outline",
    "datasketch-v2.md": "material/swap-horizontal",
    "benchmarks.md": "material/speedometer",
    "benchmark-evidence.md": "material/clipboard-check-outline",
    "public-corpora.md": "material/earth",
    "public-corpus-evidence.md": "material/file-check-outline",
    "similarity-family-evaluation.md": "material/compare",
    "sharding-evaluation.md": "material/call-split",
    "compatibility.md": "material/handshake-outline",
    "platforms.md": "material/laptop",
    "releasing.md": "material/rocket-launch",
    "maintenance.md": "material/wrench",
    "dependency-policy.md": "material/package-variant",
    "ci-supply-chain.md": "material/shield-check",
}
RELEASE_NOTES_ICON = "material/tag"


def on_page_markdown(
    markdown: str, *, page: Any, config: Any, files: Any, **_: Any
) -> str:
    """Point links that leave ``docs/`` at the repository on GitHub.

    The Markdown sources are also read directly on GitHub, where links such as
    ``../CHANGELOG.md`` or ``../benchmarks/results/...`` resolve. The published
    site only contains ``docs/``, so those targets are rewritten to the
    repository's ``main`` branch instead of becoming broken links.
    """

    source = page.file.src_uri
    icon = PAGE_ICONS.get(source)
    if icon is None and source.startswith("releases/"):
        icon = RELEASE_NOTES_ICON
    if icon is not None:
        page.meta.setdefault("icon", icon)

    source_dir = posixpath.dirname(source)

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
