#!/usr/bin/env python3
"""Streaming structured-record candidate generation and labeled evaluation."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import sys
import time
import unicodedata
from collections import Counter
from collections.abc import Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from pari import DedupeIndex, __version__

REPORT_SCHEMA_VERSION = 1
WORKLOAD_NAME = "entity-record-matching"
WORD_PATTERN = re.compile(r"[^\W_]+", re.UNICODE)
NON_ALNUM_PATTERN = re.compile(r"[^\w]+", re.UNICODE)


@dataclass(frozen=True, slots=True)
class RecordRef:
    key: int
    identity: str | int
    label: str | int | None
    offset: int


def normalized_words(value: str) -> list[str]:
    return WORD_PATTERN.findall(unicodedata.normalize("NFKC", value).casefold())


def compact(value: str) -> str:
    return NON_ALNUM_PATTERN.sub("", unicodedata.normalize("NFKC", value).casefold())


def character_ngrams(value: str, size: int = 3) -> list[str]:
    normalized = " ".join(normalized_words(value))
    if not normalized:
        return []
    padded = f"^{normalized}$"
    if len(padded) <= size:
        return [padded]
    return [padded[start : start + size] for start in range(len(padded) - size + 1)]


def add_word_features(features: set[bytes], prefix: str, value: str) -> None:
    for token in normalized_words(value):
        features.add(f"{prefix}:word:{token}".encode())


def add_ngram_features(features: set[bytes], prefix: str, value: str) -> None:
    for ngram in character_ngrams(value):
        features.add(f"{prefix}:gram:{ngram}".encode())


def customer_features(record: dict[str, Any]) -> list[bytes]:
    features: set[bytes] = set()
    name = optional_string(record, "name")
    address = optional_string(record, "address")
    email = optional_string(record, "email")
    phone = optional_string(record, "phone")
    if name:
        add_word_features(features, "name", name)
        add_ngram_features(features, "name", name)
    if address:
        add_word_features(features, "address", address)
    if email:
        normalized = email.strip().casefold()
        features.add(f"email:exact:{normalized}".encode())
        domain = normalized.rpartition("@")[2]
        if domain:
            features.add(f"email:domain:{domain}".encode())
    if phone:
        digits = "".join(character for character in phone if character.isdigit())
        if digits:
            features.add(f"phone:exact:{digits[-10:]}".encode())
    return sorted(features)


def product_features(record: dict[str, Any]) -> list[bytes]:
    features: set[bytes] = set()
    title = optional_string(record, "title")
    brand = optional_string(record, "brand")
    sku = optional_string(record, "sku")
    category = optional_string(record, "category")
    if title:
        add_word_features(features, "title", title)
    if brand:
        normalized = compact(brand)
        if normalized:
            features.add(f"brand:exact:{normalized}".encode())
    if sku:
        normalized = compact(sku)
        if normalized:
            features.add(f"sku:exact:{normalized}".encode())
    if category:
        add_word_features(features, "category", category)
    return sorted(features)


def features_for(
    record: dict[str, Any], profile: Literal["customer", "product"]
) -> list[bytes]:
    features = (
        customer_features(record) if profile == "customer" else product_features(record)
    )
    if not features:
        raise ValueError(f"record {record.get('id')!r} has no usable {profile} fields")
    return features


def optional_string(record: dict[str, Any], field: str) -> str | None:
    value = record.get(field)
    if value is None:
        return None
    if not isinstance(value, str):
        raise ValueError(f"optional field {field!r} must be a string")  # noqa: TRY004
    return value


def required_identity(
    record: dict[str, Any], field: str, line_number: int
) -> str | int:
    value = record.get(field)
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ValueError(  # noqa: TRY004
            f"field {field!r} must be a string or integer at line {line_number}"
        )
    return value


def optional_label(
    record: dict[str, Any], field: str | None, line_number: int
) -> str | int | None:
    if field is None:
        return None
    value = record.get(field)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise ValueError(  # noqa: TRY004
            f"field {field!r} must be a string, integer, or null at line {line_number}"
        )
    return value


def iter_records(
    path: Path,
    profile: Literal["customer", "product"],
    id_field: str,
    label_field: str | None,
    limit: int | None,
) -> Iterator[tuple[RecordRef, list[bytes]]]:
    before = path.stat()
    seen: set[str | int] = set()
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
            identity = required_identity(record, id_field, line_number)
            if identity in seen:
                raise ValueError(
                    f"duplicate record identity {identity!r} at line {line_number}"
                )
            seen.add(identity)
            label = optional_label(record, label_field, line_number)
            try:
                features = features_for(record, profile)
            except ValueError as error:
                raise ValueError(
                    f"invalid {profile} features at line {line_number}: {error}"
                ) from error
            yield (
                RecordRef(key=key, identity=identity, label=label, offset=offset),
                features,
            )
            key += 1

    after = path.stat()
    if before.st_size != after.st_size or before.st_mtime_ns != after.st_mtime_ns:
        raise ValueError(f"input changed during workload: {path}")


def parse_json_object(line: bytes, path: Path, line_number: int) -> dict[str, Any]:
    try:
        value = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(
            f"invalid JSON in {path} at line {line_number}: {error}"
        ) from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path} at line {line_number}")  # noqa: TRY004
    return value


def reference_json(reference: RecordRef, label_field: str | None) -> dict[str, Any]:
    value: dict[str, Any] = {"id": reference.identity, "key": reference.key}
    if label_field is not None:
        value[label_field] = reference.label
    return value


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


def true_pair_count(records: Sequence[RecordRef]) -> int:
    counts = Counter(record.label for record in records if record.label is not None)
    return sum(count * (count - 1) // 2 for count in counts.values())


def match_command(args: argparse.Namespace) -> None:
    output_paths = [args.pairs_output, args.groups_output, args.metrics_output]
    if args.index is not None:
        output_paths.append(args.index)
    prepare_new_paths(output_paths)

    started = time.perf_counter()
    all_records: list[RecordRef] = []

    def tracked_inputs() -> Iterator[tuple[RecordRef, list[bytes]]]:
        for reference, features in iter_records(
            args.input.resolve(),
            args.profile,
            args.id_field,
            args.label_field,
            args.limit,
        ):
            all_records.append(reference)
            yield reference, features

    index = DedupeIndex[RecordRef](
        None,
        threshold=args.threshold,
        num_perm=args.num_perm,
        seed=args.seed,
        batch_size=args.batch_size,
        threads=args.threads,
        path=args.index,
        backend="local" if args.index is not None else "memory",
    )
    try:
        added = index.add_many_features(tracked_inputs())
        candidate_pairs = sorted(
            index.candidate_pairs(), key=lambda pair: (pair[0].key, pair[1].key)
        )
        candidate_groups = index.candidate_groups()
        index.sync()
    finally:
        index.close()
    elapsed = time.perf_counter() - started
    if len(all_records) != added:
        raise ValueError("record count changed while indexing")

    with args.pairs_output.open("x", encoding="utf-8", newline="\n") as output:
        for left, right in candidate_pairs:
            same_label = (
                left.label == right.label
                if left.label is not None and right.label is not None
                else None
            )
            output.write(
                json_line(
                    {
                        "left": reference_json(left, args.label_field),
                        "right": reference_json(right, args.label_field),
                        "same_label": same_label,
                    }
                )
                + "\n"
            )

    with args.groups_output.open("x", encoding="utf-8", newline="\n") as output:
        for group in candidate_groups:
            output.write(
                json_line(
                    {
                        "members": [
                            reference_json(member, args.label_field)
                            for member in group.members
                        ],
                        "representative": reference_json(
                            group.representative, args.label_field
                        ),
                    }
                )
                + "\n"
            )

    total_pairs = added * (added - 1) // 2
    expected_true_pairs = true_pair_count(all_records) if args.label_field else None
    true_candidates = (
        sum(
            left.label is not None and left.label == right.label
            for left, right in candidate_pairs
        )
        if args.label_field
        else None
    )
    recall = (
        true_candidates / expected_true_pairs
        if expected_true_pairs and true_candidates is not None
        else None
    )
    precision = (
        true_candidates / len(candidate_pairs)
        if candidate_pairs and true_candidates is not None
        else None
    )
    reduction = 1.0 - len(candidate_pairs) / total_pairs if total_pairs else 1.0
    index_bytes = args.index.stat().st_size if args.index is not None else None
    output_bytes = args.pairs_output.stat().st_size + args.groups_output.stat().st_size

    write_json(
        args.metrics_output,
        {
            "config": {
                "batch_size": args.batch_size,
                "id_field": args.id_field,
                "input": str(args.input.resolve()),
                "label_field": args.label_field,
                "limit": args.limit,
                "num_perm": args.num_perm,
                "profile": args.profile,
                "seed": args.seed,
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
                "candidate_group_count": metric(
                    len(candidate_groups), "groups", "neutral"
                ),
                "candidate_pair_count": metric(len(candidate_pairs), "pairs", "lower"),
                "candidate_precision": metric(precision, "ratio", "higher"),
                "candidate_recall": metric(recall, "ratio", "higher"),
                "candidate_reduction_ratio": metric(reduction, "ratio", "higher"),
                "elapsed_seconds": metric(elapsed, "seconds", "lower"),
                "index_bytes": metric(index_bytes, "bytes", "lower"),
                "input_items": metric(added, "items", "neutral"),
                "items_per_second": metric(
                    added / elapsed if elapsed else 0.0, "items/second", "higher"
                ),
                "labeled_true_pair_count": metric(
                    expected_true_pairs, "pairs", "neutral"
                ),
                "output_bytes": metric(output_bytes, "bytes", "lower"),
                "process_peak_rss_bytes": metric(
                    process_peak_rss_bytes(), "bytes", "lower"
                ),
                "total_pair_count": metric(total_pairs, "pairs", "neutral"),
                "true_candidate_pair_count": metric(
                    true_candidates, "pairs", "neutral"
                ),
            },
            "schema_version": REPORT_SCHEMA_VERSION,
            "workload": WORKLOAD_NAME,
        },
    )


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


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--profile", choices=("customer", "product"), required=True)
    parser.add_argument("--id-field", default="id")
    parser.add_argument("--label-field")
    parser.add_argument("--pairs-output", type=Path, required=True)
    parser.add_argument("--groups-output", type=Path, required=True)
    parser.add_argument("--metrics-output", type=Path, required=True)
    parser.add_argument("--index", type=Path)
    parser.add_argument("--threshold", type=probability, default=0.4)
    parser.add_argument("--num-perm", type=positive, default=128)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--batch-size", type=positive, default=1024)
    parser.add_argument("--threads", type=positive)
    parser.add_argument("--limit", type=positive)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    match_command(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
