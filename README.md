A re-implementation in Rust of the PHP [QuickStatements](https://quickstatements.toolforge.org/) tool:
the background batch processing (`main --command bot`) and the web service
(`main --command server`), which serves the Vue frontend of the
[PHP repo](https://github.com/magnusmanske/quickstatements) (copied to
`public_html`, and built into the binary) and its `api.php`. It runs on
Toolforge for Wikidata and Commons, or in Docker for a Wikibase of your own.

[![Build Status](https://travis-ci.org/magnusmanske/quickstatements_rs.svg?branch=master)](https://travis-ci.org/magnusmanske/quickstatements_rs)

## Web service

`main --command server --port 8000` serves the frontend and `api.php`, with
the same actions, parameters and responses as the PHP version, so other tools
and bots keep working. Users log in with OAuth 1.0a; the bot then edits with
the credentials stored for each batch.

It is configured by the frontend config (`config_file`, the PHP
`config.json`, or `config` inline in `config_rs.json`) and these keys:

| key (`config_rs.json`) | default |
| --- | --- |
| `cookie_secret` | unset: users are logged out on every restart; at least 32 characters |
| `tmp_dir` | `quickstatements` in the system's temporary directory; temporary batches (imports opened by URL), kept for two days |

The OAuth consumer is that of the default site: `MW_CONSUMER_TOKEN` and
`MW_CONSUMER_SECRET`, or else `consumerKey`/`consumerSecret`/`agent` from the
site's `oauth.ini_file`, as in PHP. MediaWiki sends users back to
`<toolBase>api.php`; set `MW_CONSUMER_CALLBACK_OOB=1` for a consumer with a
fixed callback URL. Without a consumer, batches can be viewed, but no one can
log in. `/config.json` only shows the public part of the frontend config.

Changes on behalf of the logged-in user must come from the tool's own pages
(by `Sec-Fetch-Site`, or else `Origin`, which may also be one of
`valid_origin`). The session is kept in encrypted, `HttpOnly`, `SameSite=Lax`
cookies.

On Toolforge, `Procfile` starts the web service:
`toolforge webservice buildservice start --mount=all` (after `./build.sh`).
Set `cookie_secret` and `tmp_dir` (e.g. `/data/project/quickstatements/public_html/tmp`,
where PHP kept them) in `config_rs.json` first.

### Running your own

For a Wikibase of your own (e.g. on [wikibase.cloud](https://www.wikibase.cloud/)),
`docker compose` runs the web service, the bot and a MariaDB:

1. Register an OAuth 1.0a consumer on your wiki
   (`Special:OAuthConsumerRegistration/propose`), with the callback URL
   `<toolBase>api.php` and the grants "High-volume editing", "Edit existing
   pages" and "Create, edit, and move pages". It must be approved on the wiki.
2. Copy `docker/quickstatements.example.json` to `docker/quickstatements.json`,
   set your wiki's URLs and `toolBase` (where QuickStatements is reached, e.g.
   `http://localhost:8000/`), and a `cookie_secret` (`openssl rand -hex 32`).
   If the server reaches the wiki under a different URL than the browser,
   set `publicApi` and `oauth.publicMwOAuthUrl` to the browser's.
3. `MW_CONSUMER_TOKEN=… MW_CONSUMER_SECRET=… docker compose up -d`

All tables are in one database (`docker/schema.sql`); set `DB_PASSWORD` and
the `mysql.pass` to match for anything beyond a local test.

## Database

QuickStatements only ever talks to its own ToolsDB databases
(`…__quickstatements_p` for batches and commands, `…__quickstatements_auth` for
users and OAuth credentials; for a schema not ending in `_p`, both are in the
schema itself). It does not query the Wiki Replicas, so wiki database changes such
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
