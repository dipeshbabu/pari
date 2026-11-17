"""Feature helpers that turn text into MinHash inputs."""

from __future__ import annotations

import re
import unicodedata
from typing import Literal

from ._native import ConfigurationError

ShingleUnit = Literal["word", "char"]

_TOKEN = re.compile(r"\w+", re.UNICODE)
_WHITESPACE = re.compile(r"\s+", re.UNICODE)
# Joins tokens inside one word shingle; it cannot appear in a `\w+` token.
_JOINER = "\x1f"


def shingles(
    text: str,
    size: int = 3,
    *,
    unit: ShingleUnit = "word",
    normalize: bool = True,
) -> list[bytes]:
    """Return the unique overlapping shingles of ``text`` as UTF-8 bytes.

    ``unit="word"`` (the default) splits text into word tokens and returns each
    run of ``size`` consecutive tokens. ``unit="char"`` returns each run of
    ``size`` consecutive characters, which tolerates typos and works for text
    without spaces. Text shorter than one shingle yields a single shingle of
    everything it contains, and empty text yields no shingles.

    With ``normalize=True`` the text is NFKC-normalized and case-folded first,
    and character shingles collapse runs of whitespace to one space, so
    formatting differences do not change the result. Shingles are returned in
    first-seen order without duplicates, ready for
    ``MinHash.from_values`` or as a ``deduplicate`` ``feature``::

        deduplicate(records, feature=lambda text: shingles(text, 3))
    """

    if not isinstance(text, str):
        raise TypeError(f"text must be str, got {type(text).__name__}")
    if isinstance(size, bool) or not isinstance(size, int) or size <= 0:
        raise ConfigurationError("size must be a positive integer")
    if unit not in ("word", "char"):
        raise ConfigurationError("unit must be 'word' or 'char'")

    if normalize:
        text = unicodedata.normalize("NFKC", text).casefold()

    if unit == "word":
        tokens = _TOKEN.findall(text) if normalize else text.split()
        if not tokens:
            return []
        if len(tokens) <= size:
            return [_JOINER.join(tokens).encode()]
        windows = (
            _JOINER.join(tokens[start : start + size])
            for start in range(len(tokens) - size + 1)
        )
    else:
        if normalize:
            text = _WHITESPACE.sub(" ", text).strip()
        if not text:
            return []
        if len(text) <= size:
            return [text.encode()]
        windows = (text[start : start + size] for start in range(len(text) - size + 1))

    return [window.encode() for window in dict.fromkeys(windows)]


__all__ = ["ShingleUnit", "shingles"]
