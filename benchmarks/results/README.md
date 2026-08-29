# Versioned benchmark results

Validated public bundles belong under:

```text
benchmarks/results/<campaign-id>/<source-sha>/<profile>/
```

Do not replace an existing bundle with a newer run. Add a new source SHA or an explicitly named rerun so environment and cache differences remain visible. Every committed bundle must pass:

```bash
python scripts/benchmark_campaign.py validate path/to/bundle.json
```

Raw scheduled artifacts may remain in GitHub Actions. Commit only runs selected for a public report, with their logs, report JSON, and `bundle.json` intact.
