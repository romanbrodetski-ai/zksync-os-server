# Load Test Regression Monitoring

This project uses a dedicated `loadbase` workflow to benchmark `zksync-os-server` and track regressions over time.

The current regression path is:

- run `loadbase` in CI against a local ephemeral server
- write structured benchmark outputs into `loadbase-artifacts/`
- upload `bencher.json` to bencher.dev
- upload logs and structured outputs as GitHub artifacts for debugging

Artifact uploads are no longer used as the regression baseline source. Regression tracking is handled by bencher.dev.

## Workflow

The workflow is defined in:

- `.github/workflows/loadtest.yml`

It runs in three modes:

1. `pull_request`
2. `workflow_dispatch`
3. `push` to `main`

The CI runner starts:

- a local Anvil L1 loaded from `local-chains/v30.2/l1-state.json`
- `zksync-os-server` in ephemeral mode
- the `loadbase` benchmark client

Ephemeral mode is important here because it prevents stale on-disk DB state from interfering with benchmark startup.

## Current Benchmark Profiles

Current benchmark profiles are intentionally conservative to improve repeatability and reduce CI hangs:

- PR runs:
  - `--duration 90s`
  - `--wallets 20`
  - `--max-in-flight 15`
- `main` push and manual runs:
  - `--duration 3m`
  - `--wallets 30`
  - `--max-in-flight 25`

These values are lower than aggressive local stress settings on purpose. CI should be stable enough to produce comparable trend data, not just maximum load.

## Structured Outputs

`loadbase` writes benchmark files to `loadbase-artifacts/`:

- `summary.json`
- `report.md`
- `bencher.json`
- `metrics.json`
- `metrics.csv`

In CI:

- `summary.json` is used for the GitHub job summary
- `bencher.json` is uploaded to bencher.dev
- all of the above are uploaded as GitHub artifacts for debugging and post-run inspection

For the local usage guide and file format overview, see [loadbase/README.md](https://github.com/matter-labs/zksync-os-server/blob/main/loadbase/README.md).

## Bencher.dev Tracking

CI uploads the `loadbase` summary metrics to bencher.dev after each run.

Current exported benchmarks are:

- `sequencer/tps10_median` as `throughput`
- `sequencer/tps10_p95` as `throughput`
- `sequencer/include_latency_p50` as `seconds`
- `sequencer/receipt_timeouts` as `count`
- `sequencer/receipt_errors` as `count`

Interpretation:

- throughput is the main regression signal
- inclusion latency is tracked in seconds
- receipt timeout and receipt error counts are health indicators

Bencher is now the only regression comparison system in this workflow. If thresholds are configured in bencher.dev, it can generate alerts and PR comments. Without thresholds, it still stores history but will not raise alerts.

## GitHub Artifacts

The workflow still uploads artifacts, but only for observability and debugging.

Typical artifact contents:

- server log
- `loadbase` stdout log
- `summary.json`
- `report.md`
- `metrics.json`
- `metrics.csv`
- `bencher.json`

These artifacts are useful when:

- a run hangs or times out
- the server fails to start
- throughput drops unexpectedly and raw logs need inspection
- the bencher upload succeeds but the job output needs deeper analysis

They are not used as rolling baselines for regression calculations anymore.

## Job Summary and Monitoring

The workflow writes a GitHub job summary using `summary.json`, including:

- sample count
- median TPS10
- P95 TPS10
- median include p50 latency
- final included transaction count
- receipt timeout count
- receipt error count

This gives a quick human-readable snapshot even if you do not open artifacts or bencher.dev.

## Operational Notes

- Receipt timeouts and receipt RPC errors are intentionally non-fatal counters in CI.
- A run can complete successfully while still reporting non-zero timeout or error counts.
- If CI becomes unstable, lower `--max-in-flight` first, then `--wallets`, then `--duration`.
- If the server fails before opening port `3050`, verify that ephemeral mode is enabled and inspect the uploaded server log.

## When to Use Which Surface

Use GitHub job summary when:

- you want the quickest run snapshot

Use GitHub artifacts when:

- you need logs or raw structured outputs for debugging

Use bencher.dev when:

- you want historical trend lines
- you want branch-vs-main comparison context
- you want threshold-based regression alerts
