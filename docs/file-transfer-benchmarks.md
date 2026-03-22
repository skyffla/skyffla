# File Transfer Benchmarks

This file tracks manual local performance baselines for Skyffla file transfer.

- Machine: local machine-mode end-to-end tests on the same host
- Transport: native transfer path
- Commands:
  - single file: `SKYFFLA_PERF_FILE_MIB=2048 SKYFFLA_PERF_TIMEOUT_SECS=180 cargo test -p skyffla --test machine_end_to_end machine_native_file_transfer_reports_baseline -- --ignored --nocapture`
  - folder tree: `SKYFFLA_PERF_TREE_MIB=2048 SKYFFLA_PERF_TREE_FILES=128 SKYFFLA_PERF_TIMEOUT_SECS=180 cargo test -p skyffla --test machine_end_to_end machine_native_folder_transfer_reports_baseline -- --ignored --nocapture`

The `transfer` column is the measured download/send phase from the test harness.
When hashing overlaps the transfer, `prep` may still be non-zero while `total`
stays close to `transfer`.

| Commit Date | Commit | Scenario | Prep | Transfer | Total | Transfer Rate | End-to-End Rate | Notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 2026-03-21 | `60c1240` | File, 2048 MiB | 25.41s | 35.67s | 61.08s | 57.4 MiB/s | 33.5 MiB/s | Pre single-file overlap baseline |
| 2026-03-21 | `5c0f85e` | Folder, 2048 MiB, 128 files | 25.76s | 31.27s | 57.03s | 65.5 MiB/s | 35.9 MiB/s | Manifest-first folder path with full upfront hashing |
| 2026-03-21 | `189efe5` | File, 2048 MiB | 27.22s | 34.58s | 34.58s | 59.2 MiB/s | 59.2 MiB/s | Single-file hashing overlaps transfer |
| 2026-03-21 | `189efe5` | Folder, 2048 MiB, 128 files | 0.003s | 45.58s | 45.59s | 44.9 MiB/s | 44.9 MiB/s | Lightweight folder plan, but one transfer connection per file |
| 2026-03-22 | `338f8d7` | Folder, 2048 MiB, 128 files | 0.003s | 32.75s | 32.75s | 62.5 MiB/s | 62.5 MiB/s | Lightweight folder plan with one shared transfer connection and 4 workers |
| 2026-03-22 | `338f8d7` | Folder, 2048 MiB, 128 files, `N=1` | 0.002s | 50.78s | 50.79s | 40.3 MiB/s | 40.3 MiB/s | Single worker underutilizes the path |
| 2026-03-22 | `338f8d7` | Folder, 2048 MiB, 128 files, `N=8` | 0.003s | 33.15s | 33.16s | 61.8 MiB/s | 61.8 MiB/s | Slightly worse than `N=4` on this machine |
| 2026-03-22 | `338f8d7` | Folder, 2048 MiB, 8192 files, `N=4` | 0.039s | 38.26s | 38.33s | 53.5 MiB/s | 53.4 MiB/s | Many-small-files case; overhead is visible but still better than `N=1` |

## Takeaways

- Single-file overlap was the biggest end-to-end win: total time dropped from
  about `61s` to about `35s`.
- The first lightweight folder-overlap version removed prep cost but regressed
  raw folder throughput because every file opened a fresh transfer connection.
- Reusing one shared transfer connection across folder workers recovered the
  throughput loss and brought the `2048 MiB / 128 file` folder case down to
  about `33s` total.
- For this workload on this machine, `N=4` is the best measured folder worker
  count so far. `N=1` is much worse, and `N=8` does not beat `N=4`.
- Spreading the same `2048 MiB` across `8192` files drops folder throughput to
  about `53.5 MiB/s`, which confirms that per-file overhead matters for
  smaller files even with the shared-connection worker design.
