# cumulusState

Read-only multipurpose scanner for a Zebra v7 / v29 state database.

Opens a Zebra state database read-only and walks transactions by height. Does not write any files. Stop `zebrad` for the most reliable results. Requires the indexer feature on the state for reverse spend lookups.

## Benchmark

Full mainnet scan (Zebra v7, state v29):

```
Scanning 0..=3526439 (tip 3526439)
3526440/3526440 100% [2h<0s]
11499.32user 11115.27system 1:33:11elapsed 404%CPU
```

| Metric | Value |
|--------|-------|
| Blocks | 3,526,440 |
| Wall time | 1h 33m |
| Rate | ~630 blocks/s |
| CPU | 404% (~4 cores) |
| Peak RAM | ~1.5 GB |

Disk reads dominate. A warm page cache makes re-runs faster.

## Build

```bash
cargo build --release
```

## Run

```bash
# full chain, JSON, per-day buckets
sudo ./target/release/cumulusState \
  --cache-dir /var/lib/zebrad \
  --json \
  --per-day \
  all > results.json

# last 10,000 blocks
sudo ./target/release/cumulusState \
  --cache-dir /var/lib/zebrad \
  --blocks 10000 \
  --json \
  --per-day \
  all > results.json

# explicit range
sudo ./target/release/cumulusState \
  --cache-dir /var/lib/zebrad \
  --start 3470000 \
  --end 3480000 \
  --json \
  all > results.json
```

Stop `zebrad` before scanning for the cleanest read. If the cache dir needs root, use `sudo`.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--cache-dir` | `~/.cache/zebra` | Path to the Zebra cache directory (parent of `state/v29/...`) |
| `--network` | `mainnet` | `mainnet` or `testnet` |
| `--start` | `0` | Start height (inclusive) |
| `--end` | tip | End height (inclusive) |
| `--blocks` | — | Scan the last N blocks from tip (overrides `--start`/`--end`) |
| `--json` | off | Emit JSON instead of plain text |
| `--per-day` | off | Also emit per-day aggregates |
| `--threads` | CPU count | Parallel chunk count |
| `--out` | stdout | Write JSON to this path instead of stdout |

## Subcommands

| Command | What it collects |
|---------|------------------|
| `all` | Everything below |
| `expiry` | Expiry height distribution, zero fraction, delta histogram |
| `counts` | Block and transaction counts |
| `shielded` | Sapling spends/outputs, Orchard actions, Ironwood actions |
| `types` | Version mix (v1–v6), coinbase vs user, fully shielded / mixed / transparent |
| `addresses` | Unique transparent receivers, receiving outputs |
| `blocks` | Avg/max txs per block |
| `locktime` | Nonzero vs zero locktime, high sequence |
| `spends` | Reverse spend lookups (outpoints, Sapling/Orchard/Ironwood nullifiers) |

## Per-day windows

Day buckets follow the target block time of each era:

| Period | Height | Target | Blocks/day |
|--------|--------|--------|------------|
| Pre-Blossom | 0 – 653,599 | 150 s | 576 |
| Blossom – NU7 | 653,600 – 3,543,404 | 75 s | 1,152 |
| NU7+ | 3,543,405+ | 25 s | 3,456 |

No other upgrade changed the target. A full-chain `--per-day` run produces true calendar days across all three periods.

## Zcash block counts (current, 75 s)

| Period | Blocks |
|--------|--------|
| Day | 1,152 |
| Week | 8,064 |
| Month (30d) | 34,560 |
| Year (365d) | 420,480 |

After NU7 these triple.

## Output

JSON goes to stdout (or `--out`). Logs and the progress bar go to stderr.

Per-day buckets include `start` and `end` height so charts can label the x-axis with block numbers.

## Notes

- The indexer must be enabled on the state for `spends` reverse lookups to return hits.
- Coinbase transactions are counted separately and excluded from version totals.
- v6 is the Ironwood (NU6.3) transaction version.
- Per-day windows switch at Blossom (653,600) and NU7 (3,543,405).
