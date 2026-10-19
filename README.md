This is a partial re-implementation in Rust of the PHP [QuickStatements](http://tools.wmflabs.org/quickstatements/) tool, specifically the background batch processing.

[![Build Status](https://travis-ci.org/magnusmanske/quickstatements_rs.svg?branch=master)](https://travis-ci.org/magnusmanske/quickstatements_rs)

## Database

QuickStatements only ever talks to its own ToolsDB databases
(`…__quickstatements_p` for batches and commands, `…__quickstatements_auth` for
user names). It does not query the Wiki Replicas, so wiki database changes such
as the [2026 Commons links tables
split](https://wikitech.wikimedia.org/wiki/News/2026_Commons_links_tables_database_split)
do not affect it. Should a replica query ever be needed, use the `toolforge`
crate (re-exported as `wikimisc::toolforge`): `WikiPool::connect_for_tables`
picks the database that holds the tables, e.g. `links.commonswiki…` for
`pagelinks`, and refuses a query that would join across a split.

The pool is built by `QuickStatements::create_mysql_pool` from the `mysql`
section of the config file. Only `schema` is required; whatever else is left
out comes from the `toolforge` crate:

| key | default |
| --- | --- |
| `schema` | — (required), e.g. `s53220__quickstatements_p` |
| `host` | ToolsDB; `127.0.0.1` in local mode (`local='true'` in `replica.my.cnf`) |
| `port` | 3306; in local mode, the `toolsdb` port in the `[tunnels]` section of `replica.my.cnf` |
| `user`, `pass` | the tool's Toolforge credentials, from `TOOL_TOOLSDB_*` or `replica.my.cnf` |
| `min_connections`, `max_connections`, `keep_sec`, `max_statement_time` | 0, 10, 1s, unset |

A config that gives `host`, `user` and `pass` needs no Toolforge credentials.

The pool defaults follow the Toolforge [connection handling
policy](https://wikitech.wikimedia.org/wiki/Help:Wiki_Replicas#Connection_handling_policy),
which the previous hand-rolled pool did not: `mysql_async`'s own defaults hold
ten connections open while idle and allow up to a hundred.

## Archive

To keep the `command` table small, the commands of batches that have been
`DONE` or `STOP` for a while are moved to files, one per batch:
`<archive_dir>/<batch_id / 1000>/<batch_id>.jsonl.gz` (gzipped JSON lines,
one `command` row per line). `command_archive` holds each archived batch's
per-status counts for the batch list.

| key (`config_rs.json`) | default |
| --- | --- |
| `archive_dir` | unset: archive off, e.g. `/data/project/quickstatements/archive` |
| `archive_months` | 3 |

Rows in `command` always take precedence over an archive file. Archived
batches are read from their file for viewing; the bot restores a batch into
`command` when it is restarted, as does the PHP frontend's "reset errors".
Set the same `archive_dir` in the PHP frontend's `config.json`.

- `main --command archive` archives (and removes from `command`) every batch
  that has been DONE/STOP for `archive_months`. Restored batches are archived
  again once they are old.
- `main --command restore --id <batch>` restores a batch by hand.

`./run_job.sh [--schedule "<cron>"] <command> [arguments]` runs `main` as a
Toolforge job with the image built by `build.sh`, passing on all arguments.
It will not start a job that still exists. Run the archive daily with:

    ./run_job.sh --schedule "17 3 * * *" archive

To change the schedule, `toolforge jobs delete qs-archive` and run it again.

Toolforge NFS is not backed up: keep a copy of `archive_dir` elsewhere.


## Telemetry

The `bot` and `server` commands push their health and metrics to the
[service dashboard](https://magnustools.toolforge.org/buggregator/#/dashboard)
as components `bot` and `web` (see `wikimisc::telemetry`). It is configured
by an optional `telemetry` block in `config_rs.json`:

```json
"telemetry": { "token": "…", "url": "…" }
```

| key (`telemetry`) | default |
| --- | --- |
| `token` | `$DASHBOARD_TOKEN`; without either, telemetry is off |
| `url` | `$DASHBOARD_URL`, then the Buggregator ingest endpoint |

Besides process memory/CPU/panics and (`web`) request counts and latency, the
bot reports `qs_batches_running`, `qs_batches_finished_total`,
`qs_batch_start_failures_total`, `qs_commands_total{result=done|error}` and
`qs_backoffs_total{reason=maxlag|throttle}`, plus an event when a batch is
released after repeated errors. Telemetry never blocks or prevents startup.
