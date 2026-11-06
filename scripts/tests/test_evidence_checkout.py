from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class EvidenceCheckoutTests(unittest.TestCase):
    def test_committed_build_and_workload_provenance_identify_the_same_source(
        self,
    ) -> None:
        bundles = list(
            (ROOT / "benchmarks/results/public-corpora").glob("*/bundle.json")
        )
        self.assertTrue(bundles)
        for path in bundles:
            bundle = json.loads(path.read_text(encoding="utf-8"))
            build = json.loads(
                (path.parent / "build-provenance.json").read_text(encoding="utf-8")
            )
            self.assertTrue(bundle["git"]["clean"])
            self.assertTrue(build["clean"])
            self.assertEqual(build["git_sha"], bundle["git"]["sha"])
            for track in bundle["reports"]:
                report = json.loads(
                    (path.parent / f"{track}.json").read_text(encoding="utf-8")
                )
                self.assertEqual(
                    report["workload_metrics"]["environment"]["git_sha"],
                    bundle["git"]["sha"],
                )

    def test_committed_evidence_manifest_matches_exact_file_bytes(self) -> None:
        manifests = list(
            (ROOT / "benchmarks/results/public-corpora").glob("*/SHA256SUMS")
        )
        self.assertTrue(manifests)
        for manifest in manifests:
            for line in manifest.read_text(encoding="utf-8").splitlines():
                expected, name = line.split(maxsplit=1)
                self.assertEqual(Path(name).name, name)
                path = manifest.parent / name
                self.assertEqual(
                    hashlib.sha256(path.read_bytes()).hexdigest(), expected, str(path)
                )

    def test_autocrlf_checkout_preserves_checksummed_evidence_bytes(self) -> None:
        git = shutil.which("git")
        if git is None:
            self.skipTest("git is required for the checkout regression")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            evidence = root / "benchmarks/results/example/source/report.json"
            evidence.parent.mkdir(parents=True)
            original = b'{"status":"passed"}\n'
            evidence.write_bytes(original)
            attributes = ROOT / ".gitattributes"
            if attributes.exists():
                (root / ".gitattributes").write_bytes(attributes.read_bytes())

            def command(*args: str) -> None:
                subprocess.run(  # noqa: S603 - fixed git commands in an owned temporary fixture
                    [git, "-c", "core.autocrlf=true", *args],
                    cwd=root,
                    check=True,
                    capture_output=True,
                )

            command("init", "--quiet")
            command("add", ".")
            evidence.unlink()
            command("checkout-index", "--all", "--force")
            self.assertEqual(evidence.read_bytes(), original)
            self.assertEqual(
                hashlib.sha256(evidence.read_bytes()).digest(),
                hashlib.sha256(original).digest(),
            )


if __name__ == "__main__":
    unittest.main()
