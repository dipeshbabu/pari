#!/usr/bin/env python3
"""Reproduce public-corpus correctness and bounded workload evidence."""

from __future__ import annotations

import argparse
import csv
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import zipfile
from collections import Counter, defaultdict
from importlib import metadata
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "benchmarks/campaigns/public-corpora-v1.json"
TRACKS = ("text", "code", "entities", "audit")


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(65536), b""):
            result.update(block)
    return result.hexdigest()


def manifest_digest(path: Path) -> str:
    """Keep campaign identity stable across JSON whitespace and checkout EOLs."""
    value = json.loads(path.read_text(encoding="utf-8"))
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def write_json(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8", newline="\n") as output:
        json.dump(value, output, ensure_ascii=False, indent=2, sort_keys=True)
        output.write("\n")


def write_rows(path: Path, rows: list[dict[str, Any]]) -> None:
    with path.open("x", encoding="utf-8", newline="\n") as output:
        for row in rows:
            output.write(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n")


def read_rows(path: Path) -> list[dict[str, Any]]:
    with path.open(encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def load_module(name: str, relative: str) -> Any:
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load repository workload {relative}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def source_archive(cache: Path, source: dict[str, Any]) -> Path:
    path = cache / source["filename"]
    if path.exists():
        if path.is_symlink() or not path.is_file() or digest(path) != source["sha256"]:
            raise ValueError(f"cached corpus checksum mismatch: {path.name}")
        return path
    if not source["url"].startswith("https://"):
        raise ValueError("corpus downloads require HTTPS")
    temporary: Path | None = None
    try:
        request = urllib.request.Request(  # noqa: S310 - pinned HTTPS manifest URL
            source["url"], headers={"User-Agent": "pari-public-corpus-evaluation/1.0"}
        )
        with (
            urllib.request.urlopen(request, timeout=120) as response,  # noqa: S310 - pinned HTTPS manifest URL
            tempfile.NamedTemporaryFile(dir=cache, delete=False) as output,
        ):
            temporary = Path(output.name)
            size = 0
            while block := response.read(65536):
                size += len(block)
                if size > source["maximum_bytes"]:
                    raise ValueError("corpus archive exceeds its declared size limit")
                output.write(block)
        if digest(temporary) != source["sha256"]:
            raise ValueError(f"downloaded corpus checksum mismatch: {path.name}")
        os.link(temporary, path)
        return path
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def prepare_sms(path: Path, config: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    with zipfile.ZipFile(path) as archive:
        lines = archive.read("SMSSpamCollection").decode("utf-8").split("\n")
    rows = []
    for line in lines:
        if not line.strip():
            continue
        label, text = line.rstrip("\r").split("\t", 1)
        if label not in {"ham", "spam"}:
            raise ValueError("unexpected SMS classification")
        rows.append({"id": f"sms:{len(rows)}", "text": text})
    holdout = config["audit_holdout_items"]
    planted = config["audit_planted_items"]
    if min(holdout, planted) <= 0 or len(rows) <= holdout + planted:
        raise ValueError("SMS corpus cannot satisfy the declared audit split")
    reference = rows[:-holdout]
    queries = [
        {**row, "id": f"audit:{position}", "source_id": row["id"]}
        for position, row in enumerate([*reference[:planted], *rows[-holdout:]])
    ]
    return {"text": rows, "reference": reference, "queries": queries}


def prepare_code(
    path: Path, maximum: int
) -> tuple[list[dict[str, Any]], dict[str, int]]:
    rows = []
    skipped: Counter[str] = Counter()
    prefix = "cpython-60403a5409ff2c3f3b07dd2ca91a7a3e096839c7/"
    with tarfile.open(path, "r:gz") as archive:
        members = sorted(archive.getmembers(), key=lambda member: member.name)
        for member in members:
            relative = member.name.removeprefix(prefix)
            if not member.name.startswith(prefix) or not relative.startswith("Lib/"):
                continue
            if not member.isfile() or not relative.endswith(".py"):
                continue
            if member.size > maximum:
                skipped["oversize"] += 1
                continue
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError("unreadable source member")
            raw = stream.read()
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                skipped["non_utf8"] += 1
                continue
            if not text.strip() or "\0" in text:
                skipped["empty_or_binary"] += 1
                continue
            rows.append(
                {"repository": "python/cpython", "path": relative, "content": text}
            )
    if not rows:
        raise ValueError("no usable CPython source files")
    return rows, dict(skipped)


def prepare_entities(path: Path) -> tuple[list[dict[str, Any]], list[list[str]]]:
    rows = []
    with zipfile.ZipFile(path) as archive:
        for name, prefix in (("Abt.csv", "abt"), ("Buy.csv", "buy")):
            source = csv.DictReader(io.StringIO(archive.read(name).decode("latin-1")))
            rows.extend(
                {
                    "id": f"{prefix}:{row['id']}",
                    "title": row["name"],
                    "brand": row.get("manufacturer", ""),
                }
                for row in source
            )
        mapping = csv.DictReader(
            io.StringIO(archive.read("abt_buy_perfectMapping.csv").decode("latin-1"))
        )
        truth = sorted(
            {(f"abt:{row['idAbt']}", f"buy:{row['idBuy']}") for row in mapping}
        )
    identities = {row["id"] for row in rows}
    if len(identities) != len(rows) or any(
        a not in identities or b not in identities for a, b in truth
    ):
        raise ValueError("invalid Abt-Buy identities or ground truth")
    return rows, [list(pair) for pair in truth]


def prepare(directory: Path, cache: Path) -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    cache.mkdir(parents=True, exist_ok=True)
    sources = {
        name: source_archive(cache, spec) for name, spec in manifest["sources"].items()
    }
    directory.mkdir(parents=True, exist_ok=False)
    sms = prepare_sms(sources["sms"], manifest["config"])
    code, skipped = prepare_code(
        sources["code"], manifest["config"]["code_maximum_file_bytes"]
    )
    entities, truth = prepare_entities(sources["entities"])
    for name, rows in {**sms, "code": code, "entities": entities}.items():
        write_rows(directory / f"{name}.jsonl", rows)
    write_json(directory / "entity-truth.json", truth)
    write_json(
        directory / "prepared.json",
        {
            "campaign_manifest_sha256": manifest_digest(MANIFEST),
            "sources": manifest["sources"],
            "config": manifest["config"],
            "code_skipped": skipped,
            "items": {
                name: len(rows)
                for name, rows in {**sms, "code": code, "entities": entities}.items()
            },
            "files": {path.name: digest(path) for path in sorted(directory.iterdir())},
        },
    )


def verify_prepared(directory: Path) -> dict[str, Any]:
    prepared = json.loads((directory / "prepared.json").read_text(encoding="utf-8"))
    if prepared["campaign_manifest_sha256"] != manifest_digest(MANIFEST):
        raise ValueError("prepared corpus uses a different campaign manifest")
    for name, expected in prepared["files"].items():
        if Path(name).name != name or digest(directory / name) != expected:
            raise ValueError("prepared corpus checksum mismatch")
    return prepared


def jaccard(left: frozenset[bytes], right: frozenset[bytes]) -> float:
    union = len(left | right)
    return len(left & right) / union if union else 1.0


def components(pairs: set[tuple[int, int]]) -> list[tuple[int, ...]]:
    parent: dict[int, int] = {}

    def find(value: int) -> int:
        parent.setdefault(value, value)
        while parent[value] != value:
            parent[value] = parent[parent[value]]
            value = parent[value]
        return value

    for left, right in sorted(pairs):
        a, b = find(left), find(right)
        parent[max(a, b)] = min(a, b)
    groups: dict[int, list[int]] = defaultdict(list)
    for value in parent:
        groups[find(value)].append(value)
    return sorted(tuple(sorted(group)) for group in groups.values() if len(group) > 1)


def ratio(numerator: int, denominator: int) -> float | None:
    return numerator / denominator if denominator else None


def bipartite_quality(
    rows: list[dict[str, Any]], pairs: set[tuple[int, int]], truth: set[tuple[str, str]]
) -> dict[str, Any]:
    cross = set()
    for left, right in pairs:
        a, b = rows[left]["id"], rows[right]["id"]
        if a.startswith("buy:") and b.startswith("abt:"):
            a, b = b, a
        if a.startswith("abt:") and b.startswith("buy:"):
            cross.add((a, b))
    possible = sum(row["id"].startswith("abt:") for row in rows) * sum(
        row["id"].startswith("buy:") for row in rows
    )
    found = truth & cross
    return {
        "true_pairs": len(truth),
        "candidate_pairs": len(cross),
        "true_candidates": len(found),
        "candidate_recall": ratio(len(found), len(truth)),
        "candidate_precision": ratio(len(found), len(cross)),
        "candidate_reduction_ratio": 1 - len(cross) / possible if possible else None,
        "truth_policy": "original Abt-to-Buy pairs only; no transitive or within-source labels",
    }


def entity_sensitivity(
    rows: list[dict[str, Any]],
    features: list[frozenset[bytes]],
    truth: set[tuple[str, str]],
    config: dict[str, Any],
    directory: Path,
) -> list[dict[str, Any]]:
    from pari import Index, MinHash

    sketches = MinHash.from_batch(
        [sorted(row) for row in features],
        num_perm=config["num_perm"],
        seed=config["seed"],
        threads=config["threads"],
    )
    results = []
    for threshold in config["entity_sensitivity_thresholds"]:
        started = time.perf_counter()
        with Index.create(
            directory / f"sensitivity-{threshold}.pari",
            threshold=threshold,
            num_perm=config["num_perm"],
            seed=config["seed"],
        ) as index:
            index.add_many(list(enumerate(sketches)))
            index.sync()
            candidates = index.search_many(sketches)
        pairs = {(i, j) for i, keys in enumerate(candidates) for j in keys if i < j}
        results.append(
            {
                "threshold": threshold,
                "seconds": time.perf_counter() - started,
                **bipartite_quality(rows, pairs, truth),
            }
        )
    return results


def peak_rss() -> int | None:
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("faults", wintypes.DWORD),
                *[
                    (name, ctypes.c_size_t)
                    for name in (
                        "peak",
                        "working",
                        "peak_paged",
                        "paged",
                        "peak_nonpaged",
                        "nonpaged",
                        "pagefile",
                        "peak_pagefile",
                    )
                ],
            ]

        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.GetCurrentProcess.restype = wintypes.HANDLE
        api = ctypes.WinDLL("psapi", use_last_error=True)
        api.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(Counters),
            wintypes.DWORD,
        ]
        if api.GetProcessMemoryInfo(
            kernel.GetCurrentProcess(), ctypes.byref(counters), counters.cb
        ):
            return int(counters.peak)
        return None
    import resource

    value = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return int(value if sys.platform == "darwin" else value * 1024)


def host_environment(directory: Path) -> dict[str, Any]:
    campaign = load_module(
        "public_campaign_environment", "scripts/benchmark_campaign.py"
    )
    result = campaign.host_environment(ROOT)
    disk = shutil.disk_usage(directory)
    result["filesystem"] = {
        "path": directory.resolve().anchor,
        "total_bytes": disk.total,
        "free_bytes_after": disk.free,
        "type": None,
    }
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes

        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        physical = ctypes.c_ulonglong()
        if kernel.GetPhysicallyInstalledSystemMemory(ctypes.byref(physical)):
            result["physical_memory_bytes"] = physical.value * 1024
        filesystem = ctypes.create_unicode_buffer(64)
        kernel.GetVolumeInformationW.argtypes = [
            wintypes.LPCWSTR,
            wintypes.LPWSTR,
            wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD),
            ctypes.POINTER(wintypes.DWORD),
            ctypes.POINTER(wintypes.DWORD),
            wintypes.LPWSTR,
            wintypes.DWORD,
        ]
        if kernel.GetVolumeInformationW(
            directory.resolve().anchor,
            None,
            0,
            None,
            None,
            None,
            filesystem,
            len(filesystem),
        ):
            result["filesystem"]["type"] = filesystem.value
    result["dependencies"] = {
        name: metadata.version(name)
        for name in ("pari-similarity", "datasketch", "numpy", "scipy")
    }
    return result


def analyze(
    rows: list[dict[str, Any]],
    features: list[frozenset[bytes]],
    threshold: float,
    directory: Path,
    config: dict[str, Any],
) -> tuple[dict[str, Any], set[tuple[int, int]]]:
    import numpy as np
    from datasketch import MinHashLSH
    from pari import Index, Index64, MinHash, MinHash64
    from scipy.spatial.distance import pdist

    helper = load_module("public_interop", "benchmarks/datasketch_interop.py")
    feature_rows = [sorted(row) for row in features]
    report: dict[str, Any] = {}
    affine32_pairs: set[tuple[int, int]] = set()
    for width, sketch_type, index_type, dtype in (
        (32, MinHash, Index, np.uint32),
        (64, MinHash64, Index64, np.uint64),
    ):
        started = time.perf_counter()
        sketches = sketch_type.from_batch(
            feature_rows,
            num_perm=config["num_perm"],
            seed=config["seed"],
            threads=config["threads"],
        )
        signature_seconds = time.perf_counter() - started
        path = directory / f"analysis-{width}.pari"
        started = time.perf_counter()
        with index_type.create(
            path, threshold=threshold, num_perm=config["num_perm"], seed=config["seed"]
        ) as index:
            index.add_many(list(enumerate(sketches)))
            index.sync()
            build_seconds = time.perf_counter() - started
            stats = index.stats()
            started = time.perf_counter()
            candidates = index.search_many(sketches)
            query_seconds = time.perf_counter() - started
            if any(
                index.search(sketches[i]) != candidates[i]
                for i in range(min(16, len(rows)))
            ):
                raise RuntimeError("scalar/batch candidate parity failed")
        with index_type.open(path) as reopened:
            if reopened.search_many(sketches) != candidates:
                raise RuntimeError("persistent reopen candidate parity failed")
        pairs = {(i, j) for i, keys in enumerate(candidates) for j in keys if i < j}
        started = time.perf_counter()
        verified = {
            pair
            for pair in pairs
            if jaccard(features[pair[0]], features[pair[1]]) >= threshold
        }
        verification_seconds = time.perf_counter() - started
        started = time.perf_counter()
        verified_groups = components(verified)
        grouping_seconds = time.perf_counter() - started
        started = time.perf_counter()
        external = helper.datasketch_signatures(
            feature_rows,
            config["num_perm"],
            config["seed"],
            f"affine{width}",
            sketch_type,
            dtype,
        )
        external_signature_seconds = time.perf_counter() - started
        helper.require_signature_parity(sketches, external, f"affine{width}")
        external_index = MinHashLSH(
            num_perm=config["num_perm"], params=(stats.bands, stats.rows)
        )
        started = time.perf_counter()
        for i, sketch in enumerate(external):
            external_index.insert(i, sketch)
        external_build_seconds = time.perf_counter() - started
        started = time.perf_counter()
        external_candidates = [
            sorted(external_index.query(sketch)) for sketch in external
        ]
        external_query_seconds = time.perf_counter() - started
        if any(i not in keys for i, keys in enumerate(candidates)):
            raise RuntimeError("self-match candidate recall failed")
        key = f"affine{width}"
        report[key] = {
            "scheme": f"pari-affine{width}-v1",
            "bands": stats.bands,
            "rows": stats.rows,
            "signature_seconds": signature_seconds,
            "build_and_sync_seconds": build_seconds,
            "query_all_seconds": query_seconds,
            "items_per_second": len(rows) / (signature_seconds + build_seconds),
            "candidate_pairs": len(pairs),
            "candidate_rate": ratio(len(pairs), len(rows) * (len(rows) - 1) // 2),
            "exact_verified_pairs": len(verified),
            "verification_seconds": verification_seconds,
            "verified_components": len(verified_groups),
            "verified_grouping_seconds": grouping_seconds,
            "index_bytes": path.stat().st_size,
            "index_bytes_per_item": path.stat().st_size / len(rows),
            "signature_bytes_per_item": config["num_perm"] * width // 8,
            "bucket_p50": stats.committed_bucket_p50,
            "bucket_p95": stats.committed_bucket_p95,
            "bucket_p99": stats.committed_bucket_p99,
            "bucket_maximum": stats.committed_bucket_maximum,
            "datasketch_signature_seconds": external_signature_seconds,
            "datasketch_build_seconds": external_build_seconds,
            "datasketch_query_seconds": external_query_seconds,
            "datasketch_signature_parity": True,
            "datasketch_candidate_parity": candidates == external_candidates,
            "scalar_batch_and_reopen_parity": True,
        }
        if width == 32:
            affine32_pairs = pairs
    started = time.perf_counter()
    identical = Counter(features)
    report["exact_feature_hash_baseline"] = {
        "seconds": time.perf_counter() - started,
        "pairs": sum(n * (n - 1) // 2 for n in identical.values()),
        "groups": sum(n > 1 for n in identical.values()),
        "scope": "full corpus; already-prepared frozenset features; Python hash collisions use exact equality",
    }
    sample = features[: config["exact_baseline_sample_items"]]
    vocabulary = {feature: i for i, feature in enumerate(sorted(set().union(*sample)))}
    matrix = np.zeros((len(sample), len(vocabulary)), dtype=np.bool_)
    for i, row in enumerate(sample):
        for feature in row:
            matrix[i, vocabulary[feature]] = True
    started = time.perf_counter()
    distances = pdist(matrix, metric="jaccard")
    scipy_seconds = time.perf_counter() - started
    cursor = 0
    exact_pairs = set()
    for left in range(len(sample)):
        for right in range(left + 1, len(sample)):
            expected = jaccard(sample[left], sample[right])
            if abs((1.0 - distances[cursor]) - expected) > 1e-12:
                raise RuntimeError("SciPy/Python exact Jaccard parity failed")
            if expected >= threshold:
                exact_pairs.add((left, right))
            cursor += 1
    found = exact_pairs & affine32_pairs
    report["scipy_exact_subset"] = {
        "items": len(sample),
        "selection": "first input records in deterministic preparation order",
        "seconds": scipy_seconds,
        "feature_columns": len(vocabulary),
        "exact_pairs": len(exact_pairs),
        "affine32_candidate_recall": ratio(len(found), len(exact_pairs)),
        "python_exact_parity": True,
        "scope": "bounded exact correctness baseline; exclude from full-corpus throughput comparisons",
    }
    return report, affine32_pairs


def worker(track: str, data: Path, directory: Path) -> None:
    prepared = verify_prepared(data)
    config = prepared["config"]
    text = load_module("public_text_workload", "examples/text_workload.py")
    directory.mkdir(parents=True, exist_ok=False)
    common = [
        "--batch-size",
        str(config["batch_size"]),
        "--threads",
        str(config["threads"]),
    ]
    signature = ["--num-perm", str(config["num_perm"]), "--seed", str(config["seed"])]
    started = time.perf_counter()
    if track == "audit":
        manifest = directory / "reference-manifest.json"
        text.main(
            [
                "build-reference",
                "--input",
                str(data / "reference.jsonl"),
                "--manifest",
                str(manifest),
                "--threshold",
                str(config["text_threshold"]),
                *common,
                *signature,
            ]
        )
        build_seconds = time.perf_counter() - started
        started = time.perf_counter()
        text.main(
            [
                "audit",
                "--input",
                str(data / "queries.jsonl"),
                "--manifest",
                str(manifest),
                "--output",
                str(directory / "matches.jsonl"),
                "--metrics-output",
                str(directory / "metrics.json"),
                "--exact",
                "--exact-threshold",
                "1.0",
                *common,
            ]
        )
        audit_seconds = time.perf_counter() - started
        reference = read_rows(data / "reference.jsonl")
        queries = read_rows(data / "queries.jsonl")
        lookup: dict[frozenset[bytes], set[str]] = defaultdict(set)
        for row in reference:
            lookup[
                frozenset(text.text_shingles(row["text"], config["text_shingle_size"]))
            ].add(row["id"])
        expected = [
            lookup[
                frozenset(text.text_shingles(row["text"], config["text_shingle_size"]))
            ]
            for row in queries
        ]
        actual = read_rows(directory / "matches.jsonl")
        if len(actual) != len(expected) or any(
            {match["id"] for match in row["reference_matches"]} != truth
            for row, truth in zip(actual, expected, strict=True)
        ):
            raise RuntimeError(
                "contamination audit disagrees with exhaustive exact-feature lookup"
            )
        result = {
            "reference_items": len(reference),
            "queries": len(queries),
            "build_seconds": build_seconds,
            "audit_seconds": audit_seconds,
            "exact_queries_matched": sum(bool(value) for value in expected),
            "exact_pairs": sum(map(len, expected)),
            "exact_hash_ground_truth_parity": True,
            "planted_queries": config["audit_planted_items"],
            "source_identity_disjoint_holdout_queries": config["audit_holdout_items"],
        }
    else:
        pairs_path, groups_path = directory / "pairs.jsonl", directory / "groups.jsonl"
        outputs = [
            "--groups-output",
            str(groups_path),
            "--metrics-output",
            str(directory / "metrics.json"),
            "--index",
            str(directory / "workload.pari"),
            *common,
            *signature,
        ]
        if track == "entities":
            workload = load_module(
                "public_entity_workload", "examples/entity_matching.py"
            )
            threshold = config["entity_threshold"]
            workload.main(
                [
                    "--input",
                    str(data / "entities.jsonl"),
                    "--profile",
                    "product",
                    "--pairs-output",
                    str(pairs_path),
                    "--threshold",
                    str(threshold),
                    *outputs,
                ]
            )
        else:
            threshold = config[f"{track}_threshold"]
            outputs += [
                "--decisions-output",
                str(directory / "decisions.jsonl"),
                "--exact",
                "--exact-threshold",
                str(threshold),
            ]
            if track == "text":
                text.main(
                    [
                        "dedupe",
                        "--input",
                        str(data / "text.jsonl"),
                        "--threshold",
                        str(threshold),
                        *outputs,
                    ]
                )
            else:
                workload = load_module(
                    "public_code_workload", "examples/code_workload.py"
                )
                workload.main(
                    [
                        "--input-jsonl",
                        str(data / "code.jsonl"),
                        "--threshold",
                        str(threshold),
                        "--shingle-size",
                        str(config["code_shingle_size"]),
                        "--max-file-bytes",
                        "4194304",
                        *outputs,
                    ]
                )
        workload_seconds = time.perf_counter() - started
        workload_peak = peak_rss()
        rows = read_rows(data / f"{track}.jsonl")
        if track == "entities":
            features = [frozenset(workload.product_features(row)) for row in rows]
        elif track == "text":
            features = [
                frozenset(text.text_shingles(row["text"], config["text_shingle_size"]))
                for row in rows
            ]
        else:
            features = [
                frozenset(
                    workload.code_shingles(row["content"], config["code_shingle_size"])
                )
                for row in rows
            ]
        analysis, pairs = analyze(rows, features, threshold, directory, config)
        expected_groups = components(
            pairs
            if track == "entities"
            else {
                pair
                for pair in pairs
                if jaccard(features[pair[0]], features[pair[1]]) >= threshold
            }
        )
        actual_groups = sorted(
            tuple(sorted(member["key"] for member in row["members"]))
            for row in read_rows(groups_path)
        )
        if expected_groups != actual_groups:
            raise RuntimeError(
                "reference workload grouping disagrees with independently verified candidate graph"
            )
        result = {
            "items": len(rows),
            "threshold": threshold,
            "workload_seconds": workload_seconds,
            "workload_process_peak_rss_bytes": workload_peak,
            "workload_group_parity": True,
            "empty_feature_rows": sum(not row for row in features),
            "analysis": analysis,
        }
        if track == "entities":
            truth = {
                tuple(pair)
                for pair in json.loads((data / "entity-truth.json").read_text())
            }
            emitted = {
                (row["left"]["key"], row["right"]["key"])
                for row in read_rows(pairs_path)
            }
            if emitted != pairs:
                raise RuntimeError("entity workload candidate-pair parity failed")
            result["official_bipartite_labels"] = bipartite_quality(rows, pairs, truth)
            result["threshold_sensitivity"] = entity_sensitivity(
                rows, features, truth, config, directory
            )
        else:
            field = "text" if track == "text" else "content"
            counts = Counter(row[field] for row in rows)
            result["raw_text_exact_duplicate_pairs"] = sum(
                n * (n - 1) // 2 for n in counts.values()
            )
    result.update(
        {
            "track": track,
            "process_peak_rss_bytes": peak_rss(),
            "status": "passed",
            "workload_metrics": json.loads(
                (directory / "metrics.json").read_text(encoding="utf-8")
            ),
        }
    )
    result["output_artifacts"] = {
        path.name: {"sha256": digest(path), "bytes": path.stat().st_size}
        for path in sorted(directory.iterdir())
        if path.is_file()
    }
    write_json(directory / "report.json", result)


def git_state() -> tuple[str, bool]:
    executable = shutil.which("git")
    if executable is None:
        raise RuntimeError("git is required to record benchmark provenance")

    def command(*args: str) -> str:
        return subprocess.run(  # noqa: S603 - fixed git operation
            [executable, *args],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    return command("rev-parse", "HEAD"), not bool(command("status", "--porcelain"))


def validate_bundle(directory: Path) -> dict[str, Any]:
    bundle = json.loads((directory / "bundle.json").read_text(encoding="utf-8"))
    if (
        bundle.get("schema_version") != 1
        or bundle.get("campaign_id") != "public-corpora-v1"
    ):
        raise ValueError("unsupported public corpus evidence schema")
    if not bundle["git"]["clean"] or not re.fullmatch(
        r"[0-9a-f]{40}", bundle["git"]["sha"]
    ):
        raise ValueError("evidence must name a clean exact source commit")
    if set(bundle["reports"]) != set(TRACKS):
        raise ValueError("evidence must contain all four workload tracks")
    for track, checksum in bundle["reports"].items():
        path = directory / f"{track}.json"
        report = json.loads(path.read_text(encoding="utf-8"))
        if digest(path) != checksum or report.get("status") != "passed":
            raise ValueError("evidence report checksum/status mismatch")
        if report.get("track") != track:
            raise ValueError("evidence track identity mismatch")
        if track == "audit":
            if report.get("exact_hash_ground_truth_parity") is not True:
                raise ValueError("contamination correctness gate is missing")
        else:
            if report.get("workload_group_parity") is not True:
                raise ValueError("workload grouping correctness gate is missing")
            analysis = report.get("analysis", {})
            for scheme in ("affine32", "affine64"):
                checks = analysis.get(scheme, {})
                if (
                    checks.get("datasketch_signature_parity") is not True
                    or checks.get("scalar_batch_and_reopen_parity") is not True
                ):
                    raise ValueError("signature or storage correctness gate is missing")
            if (
                analysis.get("scipy_exact_subset", {}).get("python_exact_parity")
                is not True
            ):
                raise ValueError("exact-comparison correctness gate is missing")
    return bundle


def run(data: Path, directory: Path) -> None:
    prepared = verify_prepared(data)
    sha, clean = git_state()
    if not clean:
        raise ValueError("commit or stash changes before recording public evidence")
    directory.mkdir(parents=True, exist_ok=False)
    env = {**os.environ, "PARI_GIT_SHA": sha}
    started = time.perf_counter()
    for track in TRACKS:
        print(f"running public corpus track: {track}", flush=True)
        command = [
            sys.executable,
            str(Path(__file__).resolve()),
            "_worker",
            "--track",
            track,
            "--data",
            str(data.resolve()),
            "--directory",
            str((directory / track).resolve()),
        ]
        with (directory / f"{track}.log").open("x", encoding="utf-8") as output:
            subprocess.run(  # noqa: S603 - own interpreter and fixed repository worker
                command,
                cwd=ROOT,
                env=env,
                check=True,
                stdout=output,
                stderr=subprocess.STDOUT,
                timeout=prepared["config"]["worker_timeout_seconds"],
            )
        shutil.copyfile(directory / track / "report.json", directory / f"{track}.json")
    if git_state() != (sha, True):
        raise ValueError("source checkout changed while recording evidence")
    if verify_prepared(data) != prepared:
        raise ValueError("prepared corpus changed while recording evidence")
    write_json(
        directory / "bundle.json",
        {
            "schema_version": 1,
            "campaign_id": "public-corpora-v1",
            "git": {"sha": sha, "clean": clean},
            "environment": host_environment(directory),
            "command": sys.argv,
            "elapsed_seconds": time.perf_counter() - started,
            "preparation": prepared,
            "reports": {track: digest(directory / f"{track}.json") for track in TRACKS},
            "timing_scope": "single local host, warm filesystem cache, isolated worker per track; not a universal performance claim",
        },
    )
    validate_bundle(directory)
    print(f"validated all four public workload tracks: {directory}", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("prepare")
    prep.add_argument("--directory", type=Path, required=True)
    prep.add_argument("--cache", type=Path, required=True)
    runner = sub.add_parser("run")
    runner.add_argument("--data", type=Path, required=True)
    runner.add_argument("--directory", type=Path, required=True)
    child = sub.add_parser("_worker")
    child.add_argument("--track", choices=TRACKS, required=True)
    child.add_argument("--data", type=Path, required=True)
    child.add_argument("--directory", type=Path, required=True)
    validator = sub.add_parser("validate")
    validator.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "prepare":
        prepare(args.directory, args.cache)
    elif args.command == "run":
        run(args.data, args.directory)
    elif args.command == "_worker":
        worker(args.track, args.data, args.directory)
    else:
        validate_bundle(args.directory)


if __name__ == "__main__":
    main()
