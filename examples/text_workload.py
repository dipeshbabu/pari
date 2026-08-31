#!/usr/bin/env python3
"""Streaming text deduplication and cross-corpus contamination reference workload."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import sqlite3
import sys
import time
import unicodedata
from collections import OrderedDict
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from pari import DedupeIndex, Index, MinHash, __version__
from pari._outputs import StagedOutputs

REPORT_SCHEMA_VERSION = 1
REFERENCE_SCHEMA = "pari-text-reference-v1"
TOKEN_PATTERN = re.compile(r"\w+", re.UNICODE)


@dataclass(frozen=True, slots=True)
class DocumentRef:
    """Lightweight identity retained while raw JSONL remains on disk."""

    key: int
    identity: str | int
    offset: int


class ShingleLookup:
    """Bounded random-access shingle cache for exact verification."""

    def __init__(
        self,
        path: Path,
        text_field: str,
        shingle_size: int,
        cache_size: int,
        *,
        expected_size: int | None = None,
        expected_mtime_ns: int | None = None,
    ) -> None:
        self.path = path
        self.text_field = text_field
        self.shingle_size = shingle_size
        self.cache_size = cache_size
        self.expected_size = expected_size
        self.expected_mtime_ns = expected_mtime_ns
        self._file = path.open("rb")
        self._cache: OrderedDict[int, frozenset[bytes]] = OrderedDict()
        self.verify_unchanged()

    def shingles(self, offset: int) -> frozenset[bytes]:
        cached = self._cache.get(offset)
        if cached is not None:
            self._cache.move_to_end(offset)
            return cached

        self._file.seek(offset)
        line = self._file.readline()
        if not line:
            raise ValueError(f"no JSONL record at byte offset {offset} in {self.path}")
        record = parse_json_object(line, self.path, offset)
        text = required_text(record, self.text_field, self.path, offset)
        shingles = frozenset(text_shingles(text, self.shingle_size))
        self._cache[offset] = shingles
        if len(self._cache) > self.cache_size:
            self._cache.popitem(last=False)
        return shingles

    def verify_unchanged(self) -> None:
        current = self.path.stat()
        if self.expected_size is not None and current.st_size != self.expected_size:
            raise ValueError(f"source size changed since reference build: {self.path}")
        if (
            self.expected_mtime_ns is not None
            and current.st_mtime_ns != self.expected_mtime_ns
        ):
            raise ValueError(
                f"source modification time changed since reference build: {self.path}"
            )

    def close(self) -> None:
        self._file.close()

    def __enter__(self) -> ShingleLookup:  # noqa: PYI034
        return self

    def __exit__(
        self, exc_type: object, exc_value: object, traceback: object
    ) -> Literal[False]:
        self.close()
        return False


def normalize_tokens(text: str) -> list[str]:
    normalized = unicodedata.normalize("NFKC", text).casefold()
    return TOKEN_PATTERN.findall(normalized)


def text_shingles(text: str, size: int) -> list[bytes]:
    if size <= 0:
        raise ValueError("shingle size must be positive")
    tokens = normalize_tokens(text)
    if not tokens:
        return [b""]
    if len(tokens) < size:
        return ["\x1f".join(tokens).encode()]
    return [
        "\x1f".join(tokens[start : start + size]).encode()
        for start in range(len(tokens) - size + 1)
    ]


def exact_jaccard(left: frozenset[bytes], right: frozenset[bytes]) -> float:
    union = len(left | right)
    return 1.0 if union == 0 else len(left & right) / union


def iter_documents(
    path: Path,
    text_field: str,
    id_field: str,
    limit: int | None,
) -> Iterator[tuple[DocumentRef, str]]:
    with path.open("rb") as source:
        key = 0
        line_number = 0
        while limit is None or key < limit:
            offset = source.tell()
            line = source.readline()
            if not line:
                break
            line_number += 1
            if not line.strip():
                continue
            record = parse_json_object(line, path, line_number)
            text = required_text(record, text_field, path, line_number)
            identity = required_identity(record, id_field, path, line_number)
            yield DocumentRef(key=key, identity=identity, offset=offset), text
            key += 1


def parse_json_object(line: bytes, path: Path, location: int) -> dict[str, Any]:
    try:
        value = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"invalid JSON in {path} at {location}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path} at {location}")  # noqa: TRY004
    return value


def required_text(record: dict[str, Any], field: str, path: Path, location: int) -> str:
    value = record.get(field)
    if not isinstance(value, str):
        raise ValueError(  # noqa: TRY004
            f"field {field!r} must be a string in {path} at {location}"
        )
    return value


def required_identity(
    record: dict[str, Any], field: str, path: Path, location: int
) -> str | int:
    value = record.get(field)
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ValueError(  # noqa: TRY004
            f"field {field!r} must be a string or integer in {path} at {location}"
        )
    return value


def json_line(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def prepare_new_paths(paths: Iterable[Path]) -> None:
    paths = [path.resolve() for path in paths]
    if len(set(paths)) != len(paths):
        raise ValueError("output paths must be distinct")
    existing = [str(path) for path in paths if path.exists()]
    if existing:
        raise FileExistsError(f"refusing to overwrite existing outputs: {existing}")
    for path in paths:
        path.parent.mkdir(parents=True, exist_ok=True)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def process_peak_rss_bytes() -> int | None:
    try:
        resource_module: Any = __import__("resource")
    except ImportError:
        return None
    peak = int(resource_module.getrusage(resource_module.RUSAGE_SELF).ru_maxrss)
    return peak if sys.platform == "darwin" else peak * 1024


def environment() -> dict[str, Any]:
    return {
        "architecture": platform.machine(),
        "git_sha": os.environ.get("PARI_GIT_SHA", "unknown"),
        "operating_system": platform.system(),
        "pari_version": __version__,
        "python_version": platform.python_version(),
    }


def metric(value: float | None, unit: str, direction: str) -> dict[str, Any]:
    return {"direction": direction, "unit": unit, "value": value}


def report(
    workload: str, config: dict[str, Any], metrics: dict[str, Any]
) -> dict[str, Any]:
    return {
        "config": config,
        "engine": "pari",
        "environment": environment(),
        "generated_unix_seconds": int(time.time()),
        "metrics": metrics,
        "schema_version": REPORT_SCHEMA_VERSION,
        "workload": workload,
    }


def document_json(reference: DocumentRef) -> dict[str, Any]:
    return {"id": reference.identity, "key": reference.key}


def dedupe_command(args: argparse.Namespace) -> None:
    attributes = ("groups_output", "decisions_output", "metrics_output", "index")
    with StagedOutputs(args, attributes):
        _dedupe_command(args)


def _dedupe_command(args: argparse.Namespace) -> None:
    input_path = args.input.resolve()
    output_paths = [args.groups_output, args.decisions_output, args.metrics_output]
    if args.index is not None:
        output_paths.append(args.index)
    prepare_new_paths(output_paths)
    input_stat = input_path.stat()
    exact_pairs_checked = 0
    exact_pairs_accepted = 0

    lookup = (
        ShingleLookup(
            input_path,
            args.text_field,
            args.shingle_size,
            args.cache_size,
            expected_size=input_stat.st_size,
            expected_mtime_ns=input_stat.st_mtime_ns,
        )
        if args.exact
        else None
    )

    def exact(left: DocumentRef, right: DocumentRef) -> bool:
        nonlocal exact_pairs_checked, exact_pairs_accepted
        if lookup is None:
            return True
        exact_pairs_checked += 1
        accepted = (
            exact_jaccard(lookup.shingles(left.offset), lookup.shingles(right.offset))
            >= args.exact_threshold
        )
        exact_pairs_accepted += int(accepted)
        return bool(accepted)

    started = time.perf_counter()
    index = DedupeIndex[DocumentRef](
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
        added = index.add_many_features(
            (reference, text_shingles(text, args.shingle_size))
            for reference, text in iter_documents(
                input_path, args.text_field, args.id_field, args.limit
            )
        )
        candidate_groups = index.candidate_groups()
        result = index.result()
    finally:
        index.close()
        if lookup is not None:
            lookup.verify_unchanged()
            lookup.close()

    elapsed = time.perf_counter() - started
    candidate_items = sum(len(group.members) for group in candidate_groups)
    representative_for: dict[int, DocumentRef] = {}
    for group in result.groups:
        for member in group.members:
            representative_for[member.key] = group.representative

    args.groups_output.parent.mkdir(parents=True, exist_ok=True)
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

    decisions = sorted((*result.kept, *result.dropped), key=lambda item: item.key)
    kept_keys = {item.key for item in result.kept}
    args.decisions_output.parent.mkdir(parents=True, exist_ok=True)
    with args.decisions_output.open("x", encoding="utf-8", newline="\n") as output:
        for item in decisions:
            representative = representative_for.get(item.key, item)
            output.write(
                json_line(
                    {
                        "id": item.identity,
                        "keep": item.key in kept_keys,
                        "key": item.key,
                        "representative": document_json(representative),
                    }
                )
                + "\n"
            )

    output_bytes = (
        args.groups_output.stat().st_size + args.decisions_output.stat().st_size
    )
    index_bytes = args.index.stat().st_size if args.index is not None else None
    write_json(
        args.metrics_output,
        report(
            "text-deduplication",
            workload_config(args, input_path),
            {
                "candidate_group_count": metric(
                    len(candidate_groups), "groups", "neutral"
                ),
                "candidate_rate": metric(
                    candidate_items / added if added else 0.0, "ratio", "neutral"
                ),
                "duplicate_count": metric(result.duplicate_count, "items", "lower"),
                "duplicate_rate": metric(
                    result.duplicate_count / added if added else 0.0,
                    "ratio",
                    "neutral",
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
            },
        ),
    )


def build_reference_command(args: argparse.Namespace) -> None:
    input_path = args.input.resolve()
    manifest_path = args.manifest.resolve()
    index_path = (args.index or manifest_path.with_suffix(".pari")).resolve()
    records_path = (
        args.records_db or manifest_path.with_suffix(".records.sqlite3")
    ).resolve()
    metrics_path = (
        args.metrics_output or manifest_path.with_suffix(".metrics.json")
    ).resolve()
    prepare_new_paths([manifest_path, index_path, records_path, metrics_path])
    input_stat = input_path.stat()

    started = time.perf_counter()
    index = Index.create(
        index_path,
        threshold=args.threshold,
        num_perm=args.num_perm,
        seed=args.seed,
    )
    connection = sqlite3.connect(records_path)
    try:
        connection.execute(
            "CREATE TABLE records (key INTEGER PRIMARY KEY, identity_json TEXT NOT NULL, offset INTEGER NOT NULL)"
        )
        batch: list[tuple[DocumentRef, list[bytes]]] = []
        item_count = 0
        for reference, text in iter_documents(
            input_path, args.text_field, args.id_field, args.limit
        ):
            batch.append((reference, text_shingles(text, args.shingle_size)))
            if len(batch) == args.batch_size:
                insert_reference_batch(
                    index, connection, batch, args.num_perm, args.seed, args.threads
                )
                item_count += len(batch)
                batch.clear()
        if batch:
            insert_reference_batch(
                index, connection, batch, args.num_perm, args.seed, args.threads
            )
            item_count += len(batch)
        connection.commit()
        index.sync()
        stats = index.stats()
    finally:
        connection.close()
        index.close()

    elapsed = time.perf_counter() - started
    current_input_stat = input_path.stat()
    if (
        current_input_stat.st_size != input_stat.st_size
        or current_input_stat.st_mtime_ns != input_stat.st_mtime_ns
    ):
        raise ValueError(f"input changed during reference build: {input_path}")

    reopen_started = time.perf_counter()
    reopened = Index.open(index_path)
    try:
        if len(reopened) != item_count:
            raise ValueError("reopened reference index item count changed")
    finally:
        reopened.close()
    reopen_seconds = time.perf_counter() - reopen_started
    manifest = {
        "config": {
            "id_field": args.id_field,
            "num_perm": args.num_perm,
            "seed": args.seed,
            "shingle_size": args.shingle_size,
            "text_field": args.text_field,
            "threshold": args.threshold,
        },
        "created_unix_seconds": int(time.time()),
        "index_path": relative_path(index_path, manifest_path.parent),
        "index_sha256": sha256_file(index_path),
        "item_count": item_count,
        "records_db_path": relative_path(records_path, manifest_path.parent),
        "schema": REFERENCE_SCHEMA,
        "source_mtime_ns": input_stat.st_mtime_ns,
        "source_path": relative_path(input_path, manifest_path.parent),
        "source_size": input_stat.st_size,
    }
    write_json(manifest_path, manifest)
    write_json(
        metrics_path,
        report(
            "text-reference-build",
            workload_config(args, input_path),
            {
                "build_items_per_second": metric(
                    item_count / elapsed if elapsed else 0.0,
                    "items/second",
                    "higher",
                ),
                "elapsed_seconds": metric(elapsed, "seconds", "lower"),
                "index_bytes": metric(stats.file_bytes, "bytes", "lower"),
                "index_bytes_per_item": metric(
                    stats.file_bytes / item_count if item_count else 0.0,
                    "bytes/item",
                    "lower",
                ),
                "input_items": metric(item_count, "items", "neutral"),
                "process_peak_rss_bytes": metric(
                    process_peak_rss_bytes(), "bytes", "lower"
                ),
                "reopen_seconds": metric(reopen_seconds, "seconds", "lower"),
            },
        ),
    )


def insert_reference_batch(
    index: Index,
    connection: sqlite3.Connection,
    batch: Sequence[tuple[DocumentRef, list[bytes]]],
    num_perm: int,
    seed: int,
    threads: int | None,
) -> None:
    sketches = MinHash.from_batch(
        [features for _reference, features in batch],
        num_perm=num_perm,
        seed=seed,
        threads=threads,
    )
    index.add_many(
        [
            (reference.key, sketch)
            for (reference, _features), sketch in zip(batch, sketches)
        ]
    )
    connection.executemany(
        "INSERT INTO records(key, identity_json, offset) VALUES (?, ?, ?)",
        [
            (reference.key, json_line(reference.identity), reference.offset)
            for reference, _features in batch
        ],
    )
    connection.commit()


def audit_command(args: argparse.Namespace) -> None:
    manifest_path = args.manifest.resolve()
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    validate_manifest(manifest)
    base = manifest_path.parent
    index_path = resolve_manifest_path(manifest["index_path"], base)
    records_path = resolve_manifest_path(manifest["records_db_path"], base)
    source_path = resolve_manifest_path(manifest["source_path"], base)
    input_path = args.input.resolve()
    prepare_new_paths([args.output, args.metrics_output])
    expected_index_hash = manifest["index_sha256"]
    if sha256_file(index_path) != expected_index_hash:
        raise ValueError(f"reference index checksum mismatch: {index_path}")

    config = manifest["config"]
    reference_lookup = (
        ShingleLookup(
            source_path,
            config["text_field"],
            config["shingle_size"],
            args.cache_size,
            expected_size=manifest["source_size"],
            expected_mtime_ns=manifest["source_mtime_ns"],
        )
        if args.exact
        else None
    )
    connection = sqlite3.connect(f"file:{records_path.as_posix()}?mode=ro", uri=True)
    connection.execute("PRAGMA query_only = ON")
    open_started = time.perf_counter()
    index = Index.open(index_path)
    reopen_seconds = time.perf_counter() - open_started
    reference_count = int(manifest["item_count"])
    candidate_count = 0
    exact_match_count = 0
    matched_queries = 0
    query_count = 0
    started = time.perf_counter()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    try:
        with args.output.open("x", encoding="utf-8", newline="\n") as output:
            batch: list[tuple[DocumentRef, list[bytes]]] = []
            for reference, text in iter_documents(
                input_path, args.text_field, args.id_field, args.limit
            ):
                batch.append((reference, text_shingles(text, config["shingle_size"])))
                if len(batch) == args.batch_size:
                    counts = audit_batch(
                        output,
                        index,
                        connection,
                        reference_lookup,
                        batch,
                        config,
                        args,
                    )
                    candidate_count += counts[0]
                    exact_match_count += counts[1]
                    matched_queries += counts[2]
                    query_count += len(batch)
                    batch.clear()
            if batch:
                counts = audit_batch(
                    output,
                    index,
                    connection,
                    reference_lookup,
                    batch,
                    config,
                    args,
                )
                candidate_count += counts[0]
                exact_match_count += counts[1]
                matched_queries += counts[2]
                query_count += len(batch)
    finally:
        index.close()
        connection.close()
        if reference_lookup is not None:
            reference_lookup.verify_unchanged()
            reference_lookup.close()

    elapsed = time.perf_counter() - started
    if sha256_file(index_path) != expected_index_hash:
        raise ValueError("audit unexpectedly modified the reference index")
    denominator = query_count * reference_count
    candidate_rate = candidate_count / denominator if denominator else 0.0
    write_json(
        args.metrics_output,
        report(
            "text-cross-corpus-audit",
            {
                **workload_config(args, input_path),
                "num_perm": config["num_perm"],
                "reference_manifest": str(manifest_path),
                "reference_items": reference_count,
                "seed": config["seed"],
                "shingle_size": config["shingle_size"],
                "threshold": config["threshold"],
            },
            {
                "candidate_count": metric(candidate_count, "pairs", "lower"),
                "candidate_rate": metric(candidate_rate, "ratio", "lower"),
                "candidate_reduction": metric(1.0 - candidate_rate, "ratio", "higher"),
                "elapsed_seconds": metric(elapsed, "seconds", "lower"),
                "exact_match_count": metric(
                    exact_match_count if args.exact else None, "pairs", "neutral"
                ),
                "matched_query_count": metric(matched_queries, "queries", "neutral"),
                "output_bytes": metric(args.output.stat().st_size, "bytes", "lower"),
                "overlap_rate": metric(
                    matched_queries / query_count if query_count else 0.0,
                    "ratio",
                    "neutral",
                ),
                "process_peak_rss_bytes": metric(
                    process_peak_rss_bytes(), "bytes", "lower"
                ),
                "queries_per_second": metric(
                    query_count / elapsed if elapsed else 0.0,
                    "queries/second",
                    "higher",
                ),
                "query_count": metric(query_count, "queries", "neutral"),
                "reference_index_bytes": metric(
                    index_path.stat().st_size, "bytes", "lower"
                ),
                "reopen_seconds": metric(reopen_seconds, "seconds", "lower"),
                "unmatched_query_count": metric(
                    query_count - matched_queries, "queries", "neutral"
                ),
            },
        ),
    )


def audit_batch(
    output: Any,
    index: Index,
    connection: sqlite3.Connection,
    reference_lookup: ShingleLookup | None,
    batch: Sequence[tuple[DocumentRef, list[bytes]]],
    config: dict[str, Any],
    args: argparse.Namespace,
) -> tuple[int, int, int]:
    sketches = MinHash.from_batch(
        [features for _query, features in batch],
        num_perm=config["num_perm"],
        seed=config["seed"],
        threads=args.threads,
    )
    candidate_rows = index.search_many(sketches)
    candidate_count = sum(len(candidates) for candidates in candidate_rows)
    exact_match_count = 0
    matched_queries = 0

    for (query, features), candidates in zip(batch, candidate_rows):
        reference_records = lookup_reference_records(connection, candidates)
        query_shingles = frozenset(features)
        matches = []
        for candidate in candidates:
            identity, offset = reference_records[candidate]
            score = None
            if args.exact:
                if reference_lookup is None:
                    raise AssertionError("exact audit requires a reference lookup")
                score = exact_jaccard(query_shingles, reference_lookup.shingles(offset))
                if score < args.exact_threshold:
                    continue
                exact_match_count += 1
            match: dict[str, Any] = {"id": identity, "key": candidate}
            if score is not None:
                match["exact_jaccard"] = score
            matches.append(match)
        matched_queries += int(bool(matches))
        output.write(
            json_line(
                {
                    "candidate_count": len(candidates),
                    "matched": bool(matches),
                    "query": document_json(query),
                    "reference_matches": matches,
                }
            )
            + "\n"
        )
    return candidate_count, exact_match_count, matched_queries


def lookup_reference_records(
    connection: sqlite3.Connection, keys: Sequence[int]
) -> dict[int, tuple[str | int, int]]:
    records: dict[int, tuple[str | int, int]] = {}
    for start in range(0, len(keys), 900):
        chunk = keys[start : start + 900]
        placeholders = ",".join("?" for _ in chunk)
        query = f"SELECT key, identity_json, offset FROM records WHERE key IN ({placeholders})"
        for key, identity_json, offset in connection.execute(query, tuple(chunk)):
            records[int(key)] = (json.loads(identity_json), int(offset))
    missing = [key for key in keys if key not in records]
    if missing:
        raise ValueError(f"reference metadata missing keys: {missing[:5]}")
    return records


def validate_manifest(manifest: dict[str, Any]) -> None:
    if manifest.get("schema") != REFERENCE_SCHEMA:
        raise ValueError(
            f"unsupported reference manifest schema: {manifest.get('schema')!r}"
        )
    required = {
        "config",
        "index_path",
        "index_sha256",
        "item_count",
        "records_db_path",
        "source_mtime_ns",
        "source_path",
        "source_size",
    }
    missing = sorted(required - manifest.keys())
    if missing:
        raise ValueError(f"reference manifest is missing fields: {missing}")


def relative_path(path: Path, base: Path) -> str:
    return Path(os.path.relpath(path, base)).as_posix()


def resolve_manifest_path(value: str, base: Path) -> Path:
    path = Path(value)
    return path if path.is_absolute() else (base / path).resolve()


def workload_config(args: argparse.Namespace, input_path: Path) -> dict[str, Any]:
    return {
        "batch_size": args.batch_size,
        "exact": bool(getattr(args, "exact", False)),
        "exact_threshold": getattr(args, "exact_threshold", None),
        "id_field": args.id_field,
        "input": str(input_path),
        "limit": args.limit,
        "num_perm": getattr(args, "num_perm", None),
        "seed": getattr(args, "seed", None),
        "shingle_size": getattr(args, "shingle_size", None),
        "text_field": args.text_field,
        "threshold": getattr(args, "threshold", None),
        "threads": args.threads,
    }


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


def add_document_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--text-field", default="text")
    parser.add_argument("--id-field", default="id")
    parser.add_argument("--batch-size", type=positive, default=1024)
    parser.add_argument("--threads", type=positive)
    parser.add_argument("--limit", type=positive)


def add_signature_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--threshold", type=probability, default=0.8)
    parser.add_argument("--num-perm", type=positive, default=128)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--shingle-size", type=positive, default=3)


def add_exact_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--exact", action="store_true")
    parser.add_argument("--exact-threshold", type=probability, default=0.8)
    parser.add_argument("--cache-size", type=positive, default=4096)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    dedupe = subparsers.add_parser("dedupe", help="deduplicate one JSONL corpus")
    add_document_arguments(dedupe)
    add_signature_arguments(dedupe)
    add_exact_arguments(dedupe)
    dedupe.add_argument("--groups-output", type=Path, required=True)
    dedupe.add_argument("--decisions-output", type=Path, required=True)
    dedupe.add_argument("--metrics-output", type=Path, required=True)
    dedupe.add_argument("--index", type=Path)
    dedupe.set_defaults(handler=dedupe_command)

    build = subparsers.add_parser(
        "build-reference", help="build a reusable persistent reference index"
    )
    add_document_arguments(build)
    add_signature_arguments(build)
    build.add_argument("--manifest", type=Path, required=True)
    build.add_argument("--index", type=Path)
    build.add_argument("--records-db", type=Path)
    build.add_argument("--metrics-output", type=Path)
    build.set_defaults(handler=build_reference_command)

    audit = subparsers.add_parser(
        "audit", help="audit a query corpus against an existing reference"
    )
    add_document_arguments(audit)
    add_exact_arguments(audit)
    audit.add_argument("--manifest", type=Path, required=True)
    audit.add_argument("--output", type=Path, required=True)
    audit.add_argument("--metrics-output", type=Path, required=True)
    audit.set_defaults(handler=audit_command)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    args.handler(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
