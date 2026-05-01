#!/usr/bin/env sh
set -eu

# Runs the ignored synthetic CLI read/discovery benchmark.
# Override scale with:
#   DARC_BENCH_SESSIONS=240 DARC_BENCH_TURNS=24 DARC_BENCH_REPEAT=7 scripts/bench_cli_reads.sh

cargo test -p darc-cli --test read_scale_bench -- --ignored --nocapture
