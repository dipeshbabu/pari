from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[2] / "benchmarks/public_corpora.py"
SPEC = importlib.util.spec_from_file_location("public_corpora_test", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("cannot load public corpus benchmark")
corpora = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = corpora
SPEC.loader.exec_module(corpora)


class PublicCorpusTests(unittest.TestCase):
    def test_campaign_identity_is_portable_across_checkout_line_endings(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            first, second = (
                Path(temporary) / "first.json",
                Path(temporary) / "second.json",
            )
            first.write_bytes(b'{"value":1}\n')
            second.write_bytes(b'{\r\n  "value": 1\r\n}\r\n')
            self.assertEqual(
                corpora.manifest_digest(first), corpora.manifest_digest(second)
            )
            second.write_bytes(b'{"value":2}\n')
            self.assertNotEqual(
                corpora.manifest_digest(first), corpora.manifest_digest(second)
            )

    def test_sms_audit_retains_source_identity_and_known_overlap(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "sms.zip"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(
                    "SMSSpamCollection",
                    "".join(f"ham\tmessage {i}\n" for i in range(8)),
                )
            rows = corpora.prepare_sms(
                path, {"audit_holdout_items": 2, "audit_planted_items": 2}
            )
            self.assertEqual(len(rows["reference"]), 6)
            self.assertEqual(
                [row["source_id"] for row in rows["queries"]],
                ["sms:0", "sms:1", "sms:6", "sms:7"],
            )
            self.assertEqual(rows["queries"][0]["text"], rows["reference"][0]["text"])
            self.assertEqual(len({row["id"] for row in rows["queries"]}), 4)

    def test_entity_preparation_preserves_original_bipartite_truth(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "entities.zip"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(
                    "Abt.csv", "id,name,description,price\n1,device,desc,10\n"
                )
                archive.writestr(
                    "Buy.csv",
                    "id,name,manufacturer\n1,device,brand\n2,device variant,brand\n",
                )
                archive.writestr(
                    "abt_buy_perfectMapping.csv", "idAbt,idBuy\n1,1\n1,2\n"
                )
            rows, truth = corpora.prepare_entities(path)
            self.assertEqual([row["id"] for row in rows], ["abt:1", "buy:1", "buy:2"])
            self.assertEqual(rows[0]["brand"], "")
            self.assertEqual(truth, [["abt:1", "buy:1"], ["abt:1", "buy:2"]])

    def test_changed_cached_download_fails_without_overwriting(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "archive"
            path.write_bytes(b"unexpected bytes")
            specification = {
                "filename": "archive",
                "url": "https://example.invalid/data",
                "maximum_bytes": 100,
                "sha256": hashlib.sha256(b"expected").hexdigest(),
            }
            with mock.patch.object(corpora.urllib.request, "urlopen") as request:
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    corpora.source_archive(root, specification)
                request.assert_not_called()
            self.assertEqual(path.read_bytes(), b"unexpected bytes")

    def test_bad_download_is_never_published_in_cache(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            specification = {
                "filename": "archive",
                "url": "https://example.invalid/data",
                "maximum_bytes": 100,
                "sha256": hashlib.sha256(b"expected").hexdigest(),
            }
            with (
                mock.patch.object(
                    corpora.urllib.request, "urlopen", return_value=io.BytesIO(b"wrong")
                ),
                self.assertRaisesRegex(ValueError, "checksum mismatch"),
            ):
                corpora.source_archive(root, specification)
            self.assertEqual(list(root.iterdir()), [])

    def test_download_size_limit_applies_before_publication(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            specification = {
                "filename": "archive",
                "url": "https://example.invalid/data",
                "maximum_bytes": 2,
                "sha256": hashlib.sha256(b"large").hexdigest(),
            }
            with (
                mock.patch.object(
                    corpora.urllib.request, "urlopen", return_value=io.BytesIO(b"large")
                ),
                self.assertRaisesRegex(ValueError, "size limit"),
            ):
                corpora.source_archive(root, specification)
            self.assertEqual(list(root.iterdir()), [])

    def test_grouping_and_exact_jaccard_oracles(self) -> None:
        self.assertEqual(
            corpora.components({(3, 9), (1, 3), (15, 12), (8, 8)}),
            [(1, 3, 9), (12, 15)],
        )
        self.assertEqual(corpora.components(set()), [])
        self.assertEqual(corpora.jaccard(frozenset(), frozenset()), 1.0)
        self.assertEqual(
            corpora.jaccard(frozenset({b"a"}), frozenset({b"a", b"b"})), 0.5
        )

    def test_labeled_quality_excludes_within_source_and_implied_matches(self) -> None:
        rows = [{"id": "abt:1"}, {"id": "buy:1"}, {"id": "buy:2"}]
        result = corpora.bipartite_quality(
            rows, {(0, 1), (1, 2)}, {("abt:1", "buy:1"), ("abt:1", "buy:2")}
        )
        self.assertEqual(result["candidate_pairs"], 1)
        self.assertEqual(result["candidate_recall"], 0.5)
        self.assertEqual(result["candidate_precision"], 1.0)

    def make_bundle(self, root: Path) -> dict:
        reports = {}
        for track in corpora.TRACKS:
            report = {"track": track, "status": "passed"}
            if track == "audit":
                report["exact_hash_ground_truth_parity"] = True
            else:
                report["workload_group_parity"] = True
                report["analysis"] = {
                    scheme: {
                        "datasketch_signature_parity": True,
                        "scalar_batch_and_reopen_parity": True,
                    }
                    for scheme in ("affine32", "affine64")
                }
                report["analysis"]["scipy_exact_subset"] = {"python_exact_parity": True}
            path = root / f"{track}.json"
            corpora.write_json(path, report)
            reports[track] = corpora.digest(path)
        bundle = {
            "schema_version": 1,
            "campaign_id": "public-corpora-v1",
            "git": {"sha": "a" * 40, "clean": True},
            "reports": reports,
        }
        corpora.write_json(root / "bundle.json", bundle)
        return bundle

    def test_evidence_rejects_tampered_reports(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_bundle(root)
            corpora.validate_bundle(root)
            (root / "text.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "checksum/status"):
                corpora.validate_bundle(root)

    def test_recomputed_checksum_cannot_hide_missing_correctness_gate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bundle = self.make_bundle(root)
            path = root / "audit.json"
            path.write_text(
                json.dumps({"track": "audit", "status": "passed"}), encoding="utf-8"
            )
            bundle["reports"]["audit"] = corpora.digest(path)
            (root / "bundle.json").write_text(json.dumps(bundle), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "correctness gate"):
                corpora.validate_bundle(root)

    def test_dirty_checkout_cannot_create_canonical_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "result"
            with (
                mock.patch.object(corpora, "verify_prepared", return_value={}),
                mock.patch.object(corpora, "git_state", return_value=("a" * 40, False)),
                self.assertRaisesRegex(ValueError, "commit or stash"),
            ):
                corpora.run(Path(temporary), output)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
