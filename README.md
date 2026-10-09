This is a partial re-implementation in Rust of the PHP [QuickStatements](http://tools.wmflabs.org/quickstatements/) tool, specifically the background batch processing.

[![Build Status](https://travis-ci.org/magnusmanske/quickstatements_rs.svg?branch=master)](https://travis-ci.org/magnusmanske/quickstatements_rs)

## Database

QuickStatements only ever talks to its own ToolsDB databases
(`…__quickstatements_p` for batches and commands, `…__quickstatements_auth` for
user names). It does not query the Wiki Replicas, so wiki database changes such
as the [2026 Commons links tables
split](https://wikitech.wikimedia.org/wiki/News/2026_Commons_links_tables_database_split)
do not affect it. Should a replica query ever be needed, use
`wikimisc::toolforge_db`, which knows which cluster holds which tables
(`add_replica_pool("commons", "commonswiki", &["page", "pagelinks"])` lands on
`links.commonswiki…`) and refuses a query that would join across a split.

The pool is built by `QuickStatements::create_mysql_pool` from the `mysql`
section of the config file. Only `schema` is required:

| key | default |
| --- | --- |
| `schema` | — (required), e.g. `s53220__quickstatements_p` |
| `host` | ToolsDB on Toolforge, `127.0.0.1` elsewhere (SSH tunnel) |
| `port` | 3306 |
| `user`, `pass` | the tool's Toolforge credentials, from `TOOL_TOOLSDB_*` or `replica.my.cnf` |
| `min_connections`, `max_connections`, `keep_sec`, `max_statement_time` | 0, 10, 1s, unset |

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
  that has been DONE/STOP for `archive_months`. Run it daily as a scheduled
  job; restored batches are archived again once they are old.
- `main --command restore --id <batch>` restores a batch by hand.

Toolforge NFS is not backed up: keep a copy of `archive_dir` elsewhere.

### One-off migration of the existing table

`command` is too big to `ALTER`, `OPTIMIZE` or bulk-`DELETE` within the
ToolsDB limits, so it is replaced instead (see `src/migration.rs`).
`./run_job.sh <command> [arguments]` runs `main` as a one-off Toolforge job
with the image built by `build.sh`; it will not start a job that still exists.

1. Set `archive_dir` in both config files, push, and build (`./build.sh`).
2. `./run_job.sh migrate_copy`: archives old batches
   without touching `command`, and copies all others to `command_new` (no
   redundant `batch_id` index, compressed `json`). The first run takes hours;
   it can be interrupted and repeated. Repeat until it copies few batches.
3. Stop the bot (`toolforge jobs delete bot`) and the web service, run
   `./run_job.sh migrate_swap`, wait for it to finish, then start both again. This does a final
   pass and renames `command` to `command_old` and `command_new` to `command`.
4. `./run_job.sh migrate_verify` checks that every batch in `command_old`
   is live or archived with the same number of commands. Commands without a
   batch row are reported; copy them back with
   `INSERT INTO command SELECT * FROM command_old WHERE batch_id=…`.
5. `DROP TABLE command_old, command_migration`, remove `src/migration.rs`,
   and schedule the daily `archive` job.
