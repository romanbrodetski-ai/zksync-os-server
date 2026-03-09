# Bencher.dev Continuous Benchmarking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the fragile artifact-based performance regression system with bencher.dev, giving durable history, automatic PR comments, and dashboard visualizations — while deleting ~200 lines of custom shell scripts.

**Architecture:** `loadbase` emits a new `bencher.json` file in Bencher Metric Format (BMF) alongside its existing outputs. The GitHub Actions workflow submits this file to bencher.dev cloud using the bencher CLI, which handles baseline storage, regression detection, and PR comments. The old artifact-fetching and comparison scripts are deleted entirely.

**Tech Stack:** Rust (`serde_json`), bencher.dev cloud (free OSS tier), `bencherdev/bencher` GitHub Action, GitHub Actions secrets.

---

## Prerequisites (Manual — do before coding)

These steps require a human with repo admin access:

1. **Create bencher.dev account** at https://bencher.dev — sign in with GitHub
2. **Create a new project** named `zksync-os-server` (public, since repo is public)
3. **Generate API token** at https://bencher.dev/console/tokens
4. **Add GitHub secret** `BENCHER_API_TOKEN` to the repo at:
   `https://github.com/matter-labs/zksync-os-server/settings/secrets/actions`
5. **Note the project slug** (shown in bencher.dev dashboard URL) — you'll use it in the workflow

---

### Task 1: Add BMF JSON output to loadbase

**Files:**
- Modify: `loadbase/src/output.rs`

The existing `write_outputs()` function already writes `summary.json`. We add a `write_bencher_json()` call that maps `BenchmarkSummary` fields to Bencher Metric Format.

**Step 1: Add the BMF writer function**

In `loadbase/src/output.rs`, add this function before `write_outputs()`:

```rust
fn write_bencher_json(
    output_dir: &Path,
    summary: &BenchmarkSummary,
    metadata: &RunMetadata,
) -> anyhow::Result<()> {
    // Bencher Metric Format (BMF): flat map of benchmark-name -> measure-type -> {value}
    // Docs: https://bencher.dev/docs/reference/bencher-metric-format/
    let bmf = serde_json::json!({
        "sequencer/tps10_median": {
            "throughput": { "value": summary.median_tps10 }
        },
        "sequencer/tps10_p95": {
            "throughput": { "value": summary.p95_tps10 }
        },
        "sequencer/include_latency_p50_s": {
            "latency": { "value": summary.median_include_p50_10s_s }
        },
        "sequencer/receipt_timeouts": {
            "latency": { "value": metadata.receipt_timeouts as f64 }
        },
        "sequencer/receipt_errors": {
            "latency": { "value": metadata.receipt_errors as f64 }
        }
    });

    let path = output_dir.join("bencher.json");
    let json = serde_json::to_string_pretty(&bmf)
        .context("serialize bencher BMF")?;
    std::fs::write(&path, json)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
```

**Step 2: Call it from `write_outputs()`**

Find the block in `write_outputs()` that calls `render_summary_json()` and writes `summary.json`. After that block, add:

```rust
write_bencher_json(output_dir, &summary, metadata)
    .context("write bencher.json")?;
```

**Step 3: Verify it compiles**

```bash
cd loadbase
cargo build 2>&1
```
Expected: compiles cleanly (no new dependencies needed — `serde_json` is already used).

**Step 4: Smoke test locally**

```bash
# Won't connect to RPC but will fail after arg parsing — just check output dir is created
cargo run -- --help
```

**Step 5: Commit**

```bash
git add loadbase/src/output.rs
git commit -m "feat(loadbase): emit bencher.json in Bencher Metric Format"
```

---

### Task 2: Update the GitHub Actions workflow

**Files:**
- Modify: `.github/workflows/loadtest.yml`

**Step 1: Add bencher CLI installation step**

After the checkout step (before infrastructure setup), add:

```yaml
- name: Install bencher CLI
  uses: bencherdev/bencher@main
```

**Step 2: Replace the regression analysis step**

Find the step that calls `loadtest-compare-baseline.sh` (step 6 "Regression Analysis"). Replace it entirely with:

```yaml
- name: Track performance with bencher.dev
  env:
    BENCHER_API_TOKEN: ${{ secrets.BENCHER_API_TOKEN }}
    BENCHER_PROJECT: zksync-os-server        # ← replace with your actual project slug
    BENCHER_ADAPTER: json                    # BMF JSON format
    BENCHER_TESTBED: github-actions
  run: |
    bencher run \
      --branch "${{ github.head_ref || github.ref_name }}" \
      --if-branch "${{ github.head_ref || github.ref_name }}" \
      --else-if-branch main \
      --err \
      --github-actions "${{ secrets.GITHUB_TOKEN }}" \
      --file "${{ env.LOADBASE_OUTPUT_DIR }}/bencher.json"
```

Notes:
- `--if-branch` / `--else-if-branch main`: PR branches automatically inherit main's baseline on first run
- `--err`: makes the step fail if a regression threshold is exceeded
- `--github-actions`: posts a PR comment with results

**Step 3: Remove the artifact upload/download steps**

Delete these steps from the workflow:
- Any step uploading `loadbase-benchmark-*` artifacts (the baseline storage step)
- Any step downloading artifacts from previous runs
- The step uploading `regression.json` or `baseline.json`

Keep the step that uploads logs (`loadbase-logs-*`) — those are still useful for debugging.

**Step 4: Remove the GitHub summary step that renders regression.json**

The step that reads `regression.json` and writes to `$GITHUB_STEP_SUMMARY` can be removed — bencher.dev posts its own PR comment with richer data. (Keep any summary step that just prints the raw `summary.json` for human reference.)

**Step 5: Verify YAML is valid**

```bash
# Install actionlint if available, otherwise just check YAML syntax
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/loadtest.yml'))" && echo "YAML valid"
```

**Step 6: Commit**

```bash
git add .github/workflows/loadtest.yml
git commit -m "feat(ci): replace custom regression scripts with bencher.dev"
```

---

### Task 3: Delete the old comparison script

**Files:**
- Delete: `.github/scripts/loadtest-compare-baseline.sh`

**Step 1: Delete the file**

```bash
git rm .github/scripts/loadtest-compare-baseline.sh
```

**Step 2: Check if the scripts directory is now empty**

```bash
ls .github/scripts/
```

If empty, remove it too:

```bash
git rm -r .github/scripts/
```

**Step 3: Grep for any remaining references**

```bash
grep -r "loadtest-compare-baseline" .github/
```

Expected: no matches.

**Step 4: Commit**

```bash
git commit -m "chore: remove custom baseline comparison script (replaced by bencher.dev)"
```

---

### Task 4: Configure regression thresholds on bencher.dev

This is done in the bencher.dev dashboard UI (not in code), after the first successful run pushes baseline data.

**Step 1: After merging to main and a successful scheduled run completes:**

1. Go to `https://bencher.dev/console/projects/zksync-os-server/thresholds`
2. Create thresholds for each measure:

| Benchmark | Measure | Test | Upper/Lower Boundary | Value |
|---|---|---|---|---|
| `sequencer/tps10_median` | Throughput | t-test | Lower (regression = drop) | 0.85 (15% drop) |
| `sequencer/tps10_p95` | Throughput | t-test | Lower | 0.80 (20% drop) |
| `sequencer/include_latency_p50_s` | Latency | t-test | Upper (regression = increase) | 1.25 (25% increase) |
| `sequencer/receipt_timeouts` | Latency | % change | Upper | 1.50 (50% increase) |

**Step 2: Verify thresholds appear in the Alerts tab after the next PR run.**

---

### Task 5: Open the PR and verify end-to-end

**Step 1: Push the branch and open a PR against main**

```bash
git push origin HEAD
gh pr create --title "feat: replace custom benchmarking with bencher.dev" \
  --body "Replaces artifact-based regression detection with bencher.dev cloud. See docs/plans/2026-03-09-bencher-continuous-benchmarking-design.md"
```

**Step 2: Watch the workflow run**

In the PR, verify:
- [ ] The `loadtest` workflow runs and completes
- [ ] `bencher.json` appears in the loadbase output artifacts
- [ ] A bencher.dev bot comment appears on the PR with metrics
- [ ] The step passes (no regression on first run vs. main baseline)

**Step 3: Verify bencher.dev dashboard**

Go to `https://bencher.dev/console/projects/zksync-os-server/perf` and confirm all 5 metrics have data points.

---

## What This Deletes

| File | Lines removed |
|---|---|
| `.github/scripts/loadtest-compare-baseline.sh` | ~150 lines |
| Artifact upload/download steps in `loadtest.yml` | ~30 lines |
| Regression summary rendering in `loadtest.yml` | ~20 lines |

**Total:** ~200 lines of custom shell infrastructure replaced by ~15 lines of bencher config.

## Rollback Plan

If bencher.dev is unavailable: remove `--err` flag from the `bencher run` command so failures are non-blocking. The workflow continues, metrics are just not tracked until service resumes.
