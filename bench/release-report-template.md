## Performance report for {{TAG}}

Measured by `bench/run.py` on the release binary of this tag with a local mock
provider (no network). Budget ceilings come from `bench/budget.toml`. The raw
reports are the `perf-report-*` artifacts of the tagged commit's Perf budget run.

### CPU / RAM

| Platform | Cold start | Idle RSS | Active-session RSS | Per-turn overhead | Machine |
|---|---:|---:|---:|---:|---|
| Linux x86_64 | {{x86_64.cold_start_ms}} ms | {{x86_64.idle_rss_mb}} MB | {{x86_64.active_rss_mb}} MB | {{x86_64.per_turn_ms}} ms | {{x86_64.cpu}}, {{x86_64.cpus}} vCPU, {{x86_64.mem_total_mb}} MB RAM |
| Linux aarch64 | {{aarch64.cold_start_ms}} ms | {{aarch64.idle_rss_mb}} MB | {{aarch64.active_rss_mb}} MB | {{aarch64.per_turn_ms}} ms | {{aarch64.cpu}}, {{aarch64.cpus}} vCPU, {{aarch64.mem_total_mb}} MB RAM |

- Cold start: wall time of `tole --version`, median of 15.
- Idle RSS: resident memory of `tole serve`, idle.
- Active-session RSS: peak resident memory of a session with 30 tool-call round-trips.
- Per-turn overhead: harness cost of one provider round-trip with one tool call, provider latency excluded.

Budget ceilings for this release: see the `Headroom` column of each platform's
`run.py render` table below.

{{RENDERED_TABLES}}
