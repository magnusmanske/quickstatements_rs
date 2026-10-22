//! Turns V1 or CSV command text into command JSON, and stores temporary
//! batches (imports a user can open by URL).

use super::{random_string, AppState};
use crate::error::QsResult;
use crate::qs_parser::QuickStatementsParser;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The parser writes quantity units as Wikidata entity URIs
const WIKIDATA_ENTITY_BASE: &str = "http://www.wikidata.org/entity/";
/// Temporary batches are kept this long (as PHP's cron job did)
const TEMPORARY_MAX_AGE: Duration = Duration::from_secs(2 * 24 * 3600);

/// Parses commands in the `v1` or `csv` format. `entity_base` is the URI
/// prefix of the site's entities, for quantity units.
pub async fn parse(
    data: &str,
    format: &str,
    compress: bool,
    entity_base: &str,
) -> Result<Vec<Value>, String> {
    let mut commands = match format.trim().to_lowercase().as_str() {
        "v1" => from_v1(data, compress).await,
        "csv" => from_csv(data)?,
        format => return Err(format!("ERROR: Unknown format {format}")),
    };
    if !entity_base.is_empty() && entity_base != WIKIDATA_ENTITY_BASE {
        commands
            .iter_mut()
            .for_each(|c| rebase_units(c, entity_base));
    }
    Ok(commands)
}

/// One command per line, or separated by `||` if all on one line (e.g. from
/// a URL). Lines that cannot be parsed are skipped, as in PHP.
async fn from_v1(data: &str, compress: bool) -> Vec<Value> {
    let separator = if data.contains('\n') { "\n" } else { "||" };
    let mut parsers = vec![];
    for row in data.split(separator) {
        // An empty last value before `||` ("…|Den|||Q1|…") leaves a separator behind
        let row = row.trim().trim_start_matches(['|', '\t']);
        if row.is_empty() {
            continue;
        }
        if let Ok(parser) = QuickStatementsParser::new_from_line(row, None).await {
            parsers.push(parser);
        }
    }
    if compress {
        QuickStatementsParser::compress(&mut parsers);
    }
    parsers
        .iter()
        .filter_map(|p| p.to_json().ok())
        .flatten()
        .collect()
}

/// The CSV format (see Help:QuickStatements): a `qid` column (empty creates a
/// new item), then one column per command: `P…` statements, `qal…`
/// qualifiers and `S…`/`s…` references of the statement before, `L…`/`D…`/
/// `A…` terms, `S…` sitelinks, `#` the summary of the command before. A `-`
/// prefix removes.
fn from_csv(data: &str) -> Result<Vec<Value>, String> {
    let data = data.trim_start_matches('\u{feff}'); // Spreadsheets add a byte order mark
    let mut records = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(data.as_bytes())
        .into_records();
    let header = records
        .next()
        .ok_or("No CSV header")?
        .map_err(|e| e.to_string())?;
    let mut header = header.iter();
    // Deliberately case-sensitive: Qid, QID etc. are reserved for future use
    if header.next() != Some("qid") {
        return Err("The first CSV column must be \"qid\"".to_string());
    }
    let header: Vec<&str> = header.collect();
    if header.contains(&"qid") {
        return Err("Only the first CSV column can be \"qid\"".to_string());
    }
    if header.first() == Some(&"#") {
        return Err("A \"#\" column must follow a command column".to_string());
    }

    let mut commands: Vec<Value> = vec![];
    for record in records {
        let record = record.map_err(|e| e.to_string())?;
        let mut cells = record.iter();
        let mut item = cells.next().unwrap_or_default();
        if item.is_empty() {
            commands.push(json!({"action":"create","type":"item"}));
            item = "LAST";
        }
        // Property and value of the row's latest statement, for qualifiers and references
        let mut statement: Option<(String, Value)> = None;
        // Index of the latest references command, for `s…` columns
        let mut references: Option<usize> = None;
        for (column, value) in header.iter().zip(cells) {
            let (action, column) = match column.strip_prefix('-') {
                Some(column) => ("remove", column),
                None => ("add", *column),
            };
            let mut command = json!({"action": action, "item": item});
            let digit_after = |prefix: &str| {
                column
                    .strip_prefix(prefix)
                    .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            };
            if column.starts_with('P') {
                let datavalue = csv_datavalue(value);
                command["what"] = json!("statement");
                command["new_statement"] = json!(0);
                command["property"] = json!(column);
                command["datavalue"] = datavalue.clone();
                statement = Some((column.to_string(), datavalue));
                references = None;
            } else if let Some(what) = term(column) {
                command["what"] = json!(what);
                command["language"] = json!(&column[1..]);
                command["value"] = json!(value);
            } else if let Some(property) = column.strip_prefix("qal") {
                let (statement_property, datavalue) = statement_of(&statement, column)?;
                command["what"] = json!("qualifier");
                command["property"] = json!(statement_property);
                command["datavalue"] = datavalue;
                command["qualifier"] =
                    json!({"prop": format!("P{property}"), "value": csv_datavalue(value)});
            } else if let Some(property) = digit_after("S") {
                let (statement_property, datavalue) = statement_of(&statement, column)?;
                command["what"] = json!("sources");
                command["property"] = json!(statement_property);
                command["datavalue"] = datavalue;
                command["sources"] =
                    json!([{"prop": format!("P{property}"), "value": csv_datavalue(value)}]);
                references = Some(commands.len());
            } else if let Some(property) = digit_after("s") {
                let index = references
                    .ok_or(format!("CSV column {column} needs an S… column before it"))?;
                if let Some(sources) = commands[index]["sources"].as_array_mut() {
                    sources.push(
                        json!({"prop": format!("P{property}"), "value": csv_datavalue(value)}),
                    );
                }
                continue;
            } else if let Some(site) = column.strip_prefix('S') {
                command["what"] = json!("sitelink");
                command["site"] = json!(site);
                command["value"] = json!(value);
            } else if column == "#" {
                if let Some(last) = commands.last_mut() {
                    last["summary"] = json!(value);
                }
                continue;
            } else {
                return Err(format!("Unknown CSV column \"{column}\""));
            }
            commands.push(command);
        }
    }
    Ok(commands)
}

fn statement_of(
    statement: &Option<(String, Value)>,
    column: &str,
) -> Result<(String, Value), String> {
    statement.clone().ok_or(format!(
        "CSV column {column} needs a statement column before it"
    ))
}

fn term(column: &str) -> Option<&'static str> {
    match column.chars().next()? {
        'L' => Some("label"),
        'D' => Some("description"),
        'A' => Some("alias"),
        _ => None,
    }
}

/// A value that cannot be parsed fails when run, as in PHP
fn csv_datavalue(value: &str) -> Value {
    QuickStatementsParser::parse_datavalue(value)
        .unwrap_or_else(|| json!({"type": "unknown", "text": value.trim()}))
}

fn rebase_units(value: &mut Value, entity_base: &str) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                let rebased = match value.as_str() {
                    Some(unit) if key == "unit" => unit
                        .strip_prefix(WIKIDATA_ENTITY_BASE)
                        .map(|id| json!(format!("{entity_base}{id}"))),
                    _ => None,
                };
                match rebased {
                    Some(rebased) => *value = rebased,
                    None => rebase_units(value, entity_base),
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|v| rebase_units(v, entity_base)),
        _ => {}
    }
}

/// Stores an import, and returns the name it can be loaded with from `/tmp/`
pub fn save_temporary(dir: &Path, content: &Value) -> QsResult<String> {
    std::fs::create_dir_all(dir)?;
    let name = format!("qs_{}", random_string(20));
    std::fs::write(dir.join(&name), content.to_string())?;
    Ok(name)
}

/// The page that a tool's form opens, with `openpage=1`: shows the import
pub fn open_page(name: &str, site: &str) -> String {
    let encode = |s| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let mut url = format!("./#/batch/?tempfile={}", encode(name));
    if !site.is_empty() {
        url += &format!("&site={}", encode(site));
    }
    format!("<html><head><meta http-equiv=\"refresh\" content=\"0;URL='{url}'\" /></head><body></body></html>")
}

pub async fn serve_temporary(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    if !is_temporary_name(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(state.tmp_dir.join(&name)).await {
        Ok(content) => ([(header::CONTENT_TYPE, "application/json")], content).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Only names made by `save_temporary` (or PHP's `tempnam`), so no other file can be read
fn is_temporary_name(name: &str) -> bool {
    name.strip_prefix("qs_")
        .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Removes expired temporary batches, hourly
pub fn expire_temporary_files(dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            let dir = dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                remove_files_older_than(&dir, TEMPORARY_MAX_AGE)
            })
            .await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

fn remove_files_older_than(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified.elapsed().unwrap_or_default() >= max_age);
        if expired && is_temporary_name(&entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn v1(data: &str) -> Vec<Value> {
        parse(data, "v1", false, WIKIDATA_ENTITY_BASE)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn v1_lines_and_double_pipes() {
        let by_line = v1("Q1\tP31\tQ5\nQ2|P31|Q5\n\n").await;
        assert_eq!(by_line.len(), 2);
        assert_eq!(by_line[1]["item"], "Q2");
        assert_eq!(v1("Q1|P31|Q5||Q2|P31|Q5").await, by_line);
        // An empty description before `||` must not swallow the next command
        let commands = v1("Q1|P31|Q5|||Q2|P31|Q5").await;
        assert_eq!(commands.last().unwrap()["item"], "Q2");
    }

    #[tokio::test]
    async fn v1_skips_bad_lines_and_compresses() {
        assert_eq!(v1("nonsense\nQ1|P31|Q5").await.len(), 1);
        let commands = parse("CREATE\nLAST|Len|\"x\"", "v1", true, WIKIDATA_ENTITY_BASE)
            .await
            .unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["data"]["labels"]["en"]["value"], "x");
    }

    #[tokio::test]
    async fn unknown_format_is_an_error() {
        assert!(parse("", "xml", true, "").await.is_err());
    }

    #[tokio::test]
    async fn units_use_the_sites_entity_base() {
        let commands = parse("Q1|P1|5U11573", "v1", false, "https://wb.example/entity/")
            .await
            .unwrap();
        assert_eq!(
            commands[0]["datavalue"]["value"]["unit"],
            "https://wb.example/entity/Q11573"
        );
    }

    #[test]
    fn csv_statements_qualifiers_and_references() {
        let csv = "qid,P31,qal580,S248,s813,Len,#\nQ1,Q5,+2001-00-00T00:00:00Z/9,Q36578,+2020-01-01T00:00:00Z/11,\"Label, quoted\",why\n";
        let commands = from_csv(csv).unwrap();
        assert_eq!(commands.len(), 4);
        assert_eq!(commands[0]["what"], "statement");
        assert_eq!(commands[0]["datavalue"]["value"]["id"], "Q5");
        assert_eq!(commands[1]["what"], "qualifier");
        assert_eq!(commands[1]["qualifier"]["prop"], "P580");
        assert_eq!(commands[1]["datavalue"], commands[0]["datavalue"]);
        assert_eq!(commands[2]["what"], "sources");
        let sources = commands[2]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1]["prop"], "P813");
        assert_eq!(commands[3]["what"], "label");
        assert_eq!(commands[3]["language"], "en");
        assert_eq!(commands[3]["value"], "Label, quoted");
        assert_eq!(commands[3]["summary"], "why");
    }

    #[test]
    fn csv_create_remove_and_sitelinks() {
        let commands = from_csv("qid,-P31,Senwiki,Dde,Aen\n,Q5,Foo,Bar,Baz\n").unwrap();
        assert_eq!(commands[0], json!({"action":"create","type":"item"}));
        assert_eq!(commands[1]["action"], "remove");
        assert_eq!(commands[1]["item"], "LAST");
        assert_eq!(commands[2]["what"], "sitelink");
        assert_eq!(commands[2]["site"], "enwiki");
        assert_eq!(commands[3]["what"], "description");
        assert_eq!(commands[4]["what"], "alias");
    }

    #[test]
    fn csv_errors() {
        assert!(from_csv("").is_err());
        assert!(from_csv("Qid,P31\nQ1,Q5").is_err());
        assert!(from_csv("qid,qid\nQ1,Q2").is_err());
        assert!(from_csv("qid,#\nQ1,x").is_err());
        assert!(from_csv("qid,qal1\nQ1,Q2").is_err());
        assert!(from_csv("qid,s1\nQ1,Q2").is_err());
        assert!(from_csv("qid,X1\nQ1,Q2").is_err());
        assert!(from_csv("\u{feff}qid,P31\nQ1,Q5").is_ok());
    }

    #[test]
    fn temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let name = save_temporary(dir.path(), &json!({"status":"OK"})).unwrap();
        assert!(is_temporary_name(&name));
        assert!(dir.path().join(&name).exists());
        remove_files_older_than(dir.path(), Duration::from_secs(3600));
        assert!(dir.path().join(&name).exists());
        remove_files_older_than(dir.path(), Duration::ZERO);
        assert!(!dir.path().join(&name).exists());
    }

    #[test]
    fn temporary_names() {
        assert!(is_temporary_name("qs_aB3"));
        for name in ["qs_", "qs_../x", "config.json", "qs_a.b", "qs_a/b"] {
            assert!(!is_temporary_name(name), "{name}");
        }
    }

    #[test]
    fn open_page_encodes_its_url() {
        let page = open_page("qs_abc", "wiki'data");
        assert!(
            page.contains("URL='./#/batch/?tempfile=qs%5Fabc&site=wiki%27data'"),
            "{page}"
        );
    }
}
