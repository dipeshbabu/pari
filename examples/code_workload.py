#!/usr/bin/env python3
"""Streaming source-code corpus deduplication reference workload."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import sys
import time
import unicodedata
from collections import OrderedDict
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from pari import DedupeIndex, __version__
from pari._outputs import StagedOutputs

REPORT_SCHEMA_VERSION = 1
WORKLOAD_NAME = "code-corpus-deduplication"
DEFAULT_EXTENSIONS = (
    ".c",
    ".cc",
    ".cpp",
    ".cs",
    ".go",
    ".h",
    ".hpp",
    ".java",
    ".js",
    ".jsx",
    ".kt",
    ".kts",
    ".php",
    ".py",
    ".rb",
    ".rs",
    ".scala",
    ".sh",
    ".swift",
    ".ts",
    ".tsx",
)
TOKEN_PATTERN = re.compile(
    r"""
    [^\W\d]\w*
    |0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*
    |0[bB][01](?:_?[01])*
    |(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)+(?:[eE][+-]?\d(?:_?\d)*)?
    |\d(?:_?\d)*(?:[eE][+-]?\d(?:_?\d)*)?
    |'(?:\\.|[^'\\])*'
    |"(?:\\.|[^"\\])*"
    |===|!==|>>>|<<=|>>=|==|!=|<=|>=|=>|->|::|&&|\|\||\+\+|--|\*\*|<<|>>|:=
    |[^\s]
    """,
    re.VERBOSE | re.UNICODE,
)
NUMBER_PATTERN = re.compile(
    r"(?:0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*|0[bB][01](?:_?[01])*|"
    r"(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)+(?:[eE][+-]?\d(?:_?\d)*)?|"
    r"\d(?:_?\d)*(?:[eE][+-]?\d(?:_?\d)*)?)"
)
STRING_PATTERN = re.compile(r"'(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\"")


@dataclass(frozen=True, slots=True)
class CodeRef:
    """Stable output identity plus a reload locator for exact verification."""

    key: int
    repository: str
    path: str
    source_kind: Literal["file", "jsonl"]
    source_path: str
    locator: int
    source_size: int
    source_mtime_ns: int


@dataclass(slots=True)
class TraversalStats:
    discovered: int = 0
    accepted: int = 0
    skipped_hidden: int = 0
    skipped_extension: int = 0
    skipped_oversize: int = 0
    skipped_binary: int = 0
    skipped_empty: int = 0
    tokens: int = 0


class FeatureLookup:
    """Bounded token-shingle cache that reloads source content on demand."""

    def __init__(self, shingle_size: int, cache_size: int) -> None:
        self.shingle_size = shingle_size
        self.cache_size = cache_size
        self._cache: OrderedDict[int, frozenset[bytes]] = OrderedDict()
        self._jsonl_files: dict[str, Any] = {}

    def features(self, reference: CodeRef) -> frozenset[bytes]:
        cached = self._cache.get(reference.key)
        if cached is not None:
            self._cache.move_to_end(reference.key)
            return cached

        text = self._load_text(reference)
        features = frozenset(code_shingles(text, self.shingle_size))
        self._cache[reference.key] = features
        if len(self._cache) > self.cache_size:
            self._cache.popitem(last=False)
        return features

    def _load_text(self, reference: CodeRef) -> str:
        path = Path(reference.source_path)
        current = path.stat()
        if (
            current.st_size != reference.source_size
            or current.st_mtime_ns != reference.source_mtime_ns
        ):
            raise ValueError(f"source changed during workload: {path}")

        if reference.source_kind == "file":
            raw = path.read_bytes()
            after = path.stat()
            if (
                after.st_size != reference.source_size
                or after.st_mtime_ns != reference.source_mtime_ns
            ):
                raise ValueError(f"source changed during workload: {path}")
            return decode_code(raw, path)

        source = self._jsonl_files.get(reference.source_path)
        if source is None:
            source = path.open("rb")
            self._jsonl_files[reference.source_path] = source
        source.seek(reference.locator)
        line = source.readline()
        if not line:
            raise ValueError(
                f"no JSONL record at byte offset {reference.locator} in {path}"
            )
        record = parse_json_object(line, path, reference.locator)
        after = path.stat()
        if (
            after.st_size != reference.source_size
            or after.st_mtime_ns != reference.source_mtime_ns
        ):
            raise ValueError(f"source changed during workload: {path}")
        return required_string(record, "content", path, reference.locator)

    def close(self) -> None:
        for source in self._jsonl_files.values():
            source.close()
        self._jsonl_files.clear()

    def __enter__(self) -> FeatureLookup:  # noqa: PYI034
        return self

    def __exit__(
        self, exc_type: object, exc_value: object, traceback: object
    ) -> Literal[False]:
        self.close()
        return False


def code_tokens(text: str) -> list[str]:
    """Return language-neutral lexical tokens with literal categories normalized."""

    normalized = (
        unicodedata.normalize("NFKC", text).replace("\r\n", "\n").replace("\r", "\n")
    )
    output: list[str] = []
    for match in TOKEN_PATTERN.finditer(normalized):
        token = match.group(0)
        if NUMBER_PATTERN.fullmatch(token):
            output.append("<number>")
        elif STRING_PATTERN.fullmatch(token):
            output.append("<string>")
        else:
            output.append(token)
    return output


def code_shingles(text: str, size: int) -> list[bytes]:
    if size <= 0:
        raise ValueError("shingle size must be positive")
    tokens = code_tokens(text)
    if not tokens:
        return []
    if len(tokens) < size:
        return ["\x1f".join(tokens).encode()]
    return [
        "\x1f".join(tokens[start : start + size]).encode()
        for start in range(len(tokens) - size + 1)
    ]


def exact_jaccard(left: frozenset[bytes], right: frozenset[bytes]) -> float:
    union = len(left | right)
    return 1.0 if union == 0 else len(left & right) / union


def sorted_directory(path: Path | str) -> list[os.DirEntry[str]]:
    with os.scandir(path) as entries:
        return sorted(entries, key=lambda entry: entry.name)


def iter_sorted_files(
    root: Path, *, include_hidden: bool, stats: TraversalStats
) -> Iterator[Path]:
    """Walk one directory deterministically while retaining only one directory listing."""

    stack: list[Iterator[os.DirEntry[str]]] = [iter(sorted_directory(root))]
    while stack:
        try:
            entry = next(stack[-1])
        except StopIteration:
            stack.pop()
            continue
        if not include_hidden and entry.name.startswith("."):
            stats.skipped_hidden += 1
            continue
        if entry.is_dir(follow_symlinks=False):
            stack.append(iter(sorted_directory(entry.path)))
        elif entry.is_file(follow_symlinks=False):
            yield Path(entry.path)


def iter_directory_inputs(
    roots: Sequence[tuple[str, Path]],
    extensions: frozenset[str],
    shingle_size: int,
    max_file_bytes: int,
    include_hidden: bool,
    stats: TraversalStats,
) -> Iterator[tuple[CodeRef, list[bytes]]]:
    key = 0
    for repository, root in sorted(roots, key=lambda item: (item[0], str(item[1]))):
        for path in iter_sorted_files(root, include_hidden=include_hidden, stats=stats):
            stats.discovered += 1
            relative = path.relative_to(root).as_posix()
            if extensions and path.suffix.casefold() not in extensions:
                stats.skipped_extension += 1
                continue
            before = path.stat()
            if before.st_size > max_file_bytes:
                stats.skipped_oversize += 1
                continue
            raw = path.read_bytes()
            after = path.stat()
            if (
                before.st_size != after.st_size
                or before.st_mtime_ns != after.st_mtime_ns
            ):
                raise ValueError(f"source changed while being read: {path}")
            if b"\0" in raw:
                stats.skipped_binary += 1
                continue
            text = decode_code(raw, path)
            tokens = code_tokens(text)
            if not tokens:
                stats.skipped_empty += 1
                continue
            features = shingles_from_tokens(tokens, shingle_size)
            reference = CodeRef(
                key=key,
                repository=repository,
                path=relative,
                source_kind="file",
                source_path=str(path.resolve()),
                locator=0,
                source_size=after.st_size,
                source_mtime_ns=after.st_mtime_ns,
            )
            stats.accepted += 1
            stats.tokens += len(tokens)
            yield reference, features
            key += 1


def iter_jsonl_inputs(
    path: Path,
    shingle_size: int,
    max_file_bytes: int,
    stats: TraversalStats,
) -> Iterator[tuple[CodeRef, list[bytes]]]:
    input_stat = path.stat()
    seen: set[tuple[str, str]] = set()
    with path.open("rb") as source:
        key = 0
        line_number = 0
        while True:
            offset = source.tell()
            line = source.readline()
            if not line:
                break
            line_number += 1
            if not line.strip():
                continue
            stats.discovered += 1
            if len(line) > max_file_bytes:
                stats.skipped_oversize += 1
                continue
            record = parse_json_object(line, path, line_number)
            repository = required_string(record, "repository", path, line_number)
            relative = required_string(record, "path", path, line_number)
            content = required_string(record, "content", path, line_number)
            identity = (repository, relative)
            if identity in seen:
                raise ValueError(
                    f"duplicate repository/path identity {identity!r} at line {line_number}"
                )
            seen.add(identity)
            tokens = code_tokens(content)
            if not tokens:
                stats.skipped_empty += 1
                continue
            reference = CodeRef(
                key=key,
                repository=repository,
                path=relative,
                source_kind="jsonl",
                source_path=str(path.resolve()),
                locator=offset,
                source_size=input_stat.st_size,
                source_mtime_ns=input_stat.st_mtime_ns,
            )
            stats.accepted += 1
            stats.tokens += len(tokens)
            yield reference, shingles_from_tokens(tokens, shingle_size)
            key += 1

    current = path.stat()
    if (
        current.st_size != input_stat.st_size
        or current.st_mtime_ns != input_stat.st_mtime_ns
    ):
        raise ValueError(f"input changed during workload: {path}")


def shingles_from_tokens(tokens: Sequence[str], size: int) -> list[bytes]:
    if len(tokens) < size:
        return ["\x1f".join(tokens).encode()]
    return [
        "\x1f".join(tokens[start : start + size]).encode()
        for start in range(len(tokens) - size + 1)
    ]


def parse_json_object(line: bytes, path: Path, location: int) -> dict[str, Any]:
    try:
        value = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"invalid JSON in {path} at {location}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path} at {location}")  # noqa: TRY004
    return value


def required_string(
    record: dict[str, Any], field: str, path: Path, location: int
) -> str:
    value = record.get(field)
    if not isinstance(value, str) or not value:
        raise ValueError(
            f"field {field!r} must be a non-empty string in {path} at {location}"
        )
    return value


def decode_code(raw: bytes, path: Path) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"source file is not UTF-8: {path}: {error}") from error


def document_json(reference: CodeRef) -> dict[str, Any]:
    return {
        "key": reference.key,
        "path": reference.path,
        "repository": reference.repository,
    }


def json_line(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def prepare_new_paths(paths: Sequence[Path]) -> None:
    resolved = [path.resolve() for path in paths]
    if len(set(resolved)) != len(resolved):
        raise ValueError("output paths must be distinct")
    existing = [str(path) for path in resolved if path.exists()]
    if existing:
        raise FileExistsError(f"refusing to overwrite existing outputs: {existing}")
    for path in resolved:
        path.parent.mkdir(parents=True, exist_ok=True)


def process_peak_rss_bytes() -> int | None:
    try:
        resource_module: Any = __import__("resource")
    except ImportError:
        return None
    peak = int(resource_module.getrusage(resource_module.RUSAGE_SELF).ru_maxrss)
    return peak if sys.platform == "darwin" else peak * 1024


def metric(value: float | None, unit: str, direction: str) -> dict[str, Any]:
    return {"direction": direction, "unit": unit, "value": value}


def write_json(path: Path, value: Any) -> None:
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def parse_root(value: str) -> tuple[str, Path]:
    repository, separator, raw_path = value.partition("=")
    if not separator or not repository or not raw_path:
        raise argparse.ArgumentTypeError("root must use REPOSITORY=PATH")
    path = Path(raw_path).resolve()
    if not path.is_dir():
        raise argparse.ArgumentTypeError(f"root is not a directory: {path}")
    return repository, path


def normalized_extensions(values: Sequence[str] | None) -> frozenset[str]:
    selected = values if values is not None else DEFAULT_EXTENSIONS
    output = set()
    for value in selected:
        extension = value.casefold()
        output.add(extension if extension.startswith(".") else f".{extension}")
    return frozenset(output)


def positive(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def probability(value: str) -> float:
    parsed = float(value)
    if not 0.0 < parsed <= 1.0:
        raise argparse.ArgumentTypeError("must be in (0, 1]")
    return parsed


def dedupe_command(args: argparse.Namespace) -> None:
    attributes = ("groups_output", "decisions_output", "metrics_output", "index")
    with StagedOutputs(args, attributes):
        _dedupe_command(args)


def _dedupe_command(args: argparse.Namespace) -> None:
    output_paths = [args.groups_output, args.decisions_output, args.metrics_output]
    if args.index is not None:
        output_paths.append(args.index)
    prepare_new_paths(output_paths)

    roots = args.root or []
    repositories = [repository for repository, _path in roots]
    if len(set(repositories)) != len(repositories):
        raise ValueError("repository names supplied through --root must be unique")
    stats = TraversalStats()
    inputs = (
        iter_directory_inputs(
            roots,
            normalized_extensions(args.extension),
            args.shingle_size,
            args.max_file_bytes,
            args.include_hidden,
            stats,
        )
        if roots
        else iter_jsonl_inputs(
            args.input_jsonl.resolve(),
            args.shingle_size,
            args.max_file_bytes,
            stats,
        )
    )

    exact_pairs_checked = 0
    exact_pairs_accepted = 0
    lookup = FeatureLookup(args.shingle_size, args.cache_size) if args.exact else None

    def exact(left: CodeRef, right: CodeRef) -> bool:
        nonlocal exact_pairs_checked, exact_pairs_accepted
        if lookup is None:
            return True
        exact_pairs_checked += 1
        accepted = (
            exact_jaccard(lookup.features(left), lookup.features(right))
            >= args.exact_threshold
        )
        exact_pairs_accepted += int(accepted)
        return bool(accepted)

    started = time.perf_counter()
    index = DedupeIndex[CodeRef](
        None,
        threshold=args.threshold,
        num_perm=args.num_perm,
        seed=args.seed,
        batch_size=args.batch_size,
        threads=args.threads,
        path=args.index,
        backend="local" if args.index is not None else "memory",
        exact=exact if args.exact else None,
    )
    try:
        added = index.add_many_features(inputs)
        candidate_groups = index.candidate_groups()
        result = index.result()
    finally:
        index.close()
        if lookup is not None:
            lookup.close()
    elapsed = time.perf_counter() - started

    representative_for: dict[int, CodeRef] = {}
    for group in result.groups:
        for member in group.members:
            representative_for[member.key] = group.representative

    with args.groups_output.open("x", encoding="utf-8", newline="\n") as output:
        for group in result.groups:
            output.write(
                json_line(
                    {
                        "members": [document_json(member) for member in group.members],
                        "representative": document_json(group.representative),
                    }
                )
                + "\n"
            )

    kept_keys = {item.key for item in result.kept}
    decisions = sorted((*result.kept, *result.dropped), key=lambda item: item.key)
    with args.decisions_output.open("x", encoding="utf-8", newline="\n") as output:
        for item in decisions:
            output.write(
                json_line(
                    {
                        **document_json(item),
                        "keep": item.key in kept_keys,
                        "representative": document_json(
                            representative_for.get(item.key, item)
                        ),
                    }
                )
                + "\n"
            )

    candidate_items = sum(len(group.members) for group in candidate_groups)
    output_bytes = (
        args.groups_output.stat().st_size + args.decisions_output.stat().st_size
    )
    index_bytes = args.index.stat().st_size if args.index is not None else None
    report = {
        "config": {
            "batch_size": args.batch_size,
            "exact": args.exact,
            "exact_threshold": args.exact_threshold if args.exact else None,
            "extensions": sorted(normalized_extensions(args.extension)),
            "include_hidden": args.include_hidden,
            "input_jsonl": str(args.input_jsonl.resolve())
            if args.input_jsonl
            else None,
            "max_file_bytes": args.max_file_bytes,
            "num_perm": args.num_perm,
            "roots": [
                {"path": str(path), "repository": repository}
                for repository, path in sorted(roots)
            ],
            "seed": args.seed,
            "shingle_size": args.shingle_size,
            "threshold": args.threshold,
            "threads": args.threads,
        },
        "engine": "pari",
        "environment": {
            "architecture": platform.machine(),
            "git_sha": os.environ.get("PARI_GIT_SHA", "unknown"),
            "operating_system": platform.system(),
            "pari_version": __version__,
            "python_version": platform.python_version(),
        },
        "generated_unix_seconds": int(time.time()),
        "metrics": {
            "candidate_group_count": metric(len(candidate_groups), "groups", "neutral"),
            "candidate_item_rate": metric(
                candidate_items / added if added else 0.0, "ratio", "neutral"
            ),
            "discovered_files": metric(stats.discovered, "files", "neutral"),
            "duplicate_count": metric(result.duplicate_count, "items", "lower"),
            "duplicate_rate": metric(
                result.duplicate_count / added if added else 0.0, "ratio", "neutral"
            ),
            "elapsed_seconds": metric(elapsed, "seconds", "lower"),
            "exact_pairs_accepted": metric(
                exact_pairs_accepted if args.exact else None, "pairs", "neutral"
            ),
            "exact_pairs_checked": metric(
                exact_pairs_checked if args.exact else None, "pairs", "neutral"
            ),
            "group_count": metric(result.group_count, "groups", "neutral"),
            "index_bytes": metric(index_bytes, "bytes", "lower"),
            "input_items": metric(added, "items", "neutral"),
            "items_per_second": metric(
                added / elapsed if elapsed else 0.0, "items/second", "higher"
            ),
            "output_bytes": metric(output_bytes, "bytes", "lower"),
            "process_peak_rss_bytes": metric(
                process_peak_rss_bytes(), "bytes", "lower"
            ),
            "skipped_binary": metric(stats.skipped_binary, "files", "neutral"),
            "skipped_empty": metric(stats.skipped_empty, "files", "neutral"),
            "skipped_extension": metric(stats.skipped_extension, "files", "neutral"),
            "skipped_hidden_entries": metric(
                stats.skipped_hidden, "entries", "neutral"
            ),
            "skipped_oversize": metric(stats.skipped_oversize, "files", "neutral"),
            "tokens": metric(stats.tokens, "tokens", "neutral"),
            "tokens_per_item": metric(
                stats.tokens / added if added else 0.0, "tokens/item", "neutral"
            ),
        },
        "schema_version": REPORT_SCHEMA_VERSION,
        "workload": WORKLOAD_NAME,
    }
    write_json(args.metrics_output, report)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sources = parser.add_mutually_exclusive_group(required=True)
    sources.add_argument(
        "--root",
        type=parse_root,
        action="append",
        metavar="REPOSITORY=PATH",
        help="repository root; repeat for multiple repositories",
    )
    sources.add_argument("--input-jsonl", type=Path)
    parser.add_argument("--groups-output", type=Path, required=True)
    parser.add_argument("--decisions-output", type=Path, required=True)
    parser.add_argument("--metrics-output", type=Path, required=True)
    parser.add_argument("--index", type=Path)
    parser.add_argument("--extension", action="append")
    parser.add_argument("--include-hidden", action="store_true")
    parser.add_argument("--max-file-bytes", type=positive, default=1024 * 1024)
    parser.add_argument("--threshold", type=probability, default=0.8)
    parser.add_argument("--num-perm", type=positive, default=128)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--shingle-size", type=positive, default=5)
    parser.add_argument("--batch-size", type=positive, default=1024)
    parser.add_argument("--threads", type=positive)
    parser.add_argument("--exact", action="store_true")
    parser.add_argument("--exact-threshold", type=probability, default=0.8)
    parser.add_argument("--cache-size", type=positive, default=4096)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    dedupe_command(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
