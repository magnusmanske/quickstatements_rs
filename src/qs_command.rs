use mysql as my;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct QuickStatementsCommand {
    pub id: i64,
    pub batch_id: i64,
    pub num: i64,
    pub json: Value,
    pub status: String,
    pub message: String,
    pub ts_change: String,
}

impl QuickStatementsCommand {
    pub fn new_from_row(row: my::Row) -> Self {
        Self {
            id: QuickStatementsCommand::rowvalue_as_i64(&row["id"]),
            batch_id: QuickStatementsCommand::rowvalue_as_i64(&row["batch_id"]),
            num: QuickStatementsCommand::rowvalue_as_i64(&row["num"]),
            json: match &row["json"] {
                my::Value::Bytes(x) => match serde_json::from_str(&String::from_utf8_lossy(x)) {
                    Ok(y) => y,
                    _ => json!({}),
                },
                _ => Value::Null,
            },
            status: QuickStatementsCommand::rowvalue_as_string(&row["status"]),
            message: QuickStatementsCommand::rowvalue_as_string(&row["message"]),
            ts_change: QuickStatementsCommand::rowvalue_as_string(&row["ts_change"]),
        }
    }

    fn rowvalue_as_i64(v: &my::Value) -> i64 {
        match v {
            my::Value::Int(x) => *x,
            _ => 0,
        }
    }

    fn rowvalue_as_string(v: &my::Value) -> String {
        match v {
            my::Value::Bytes(x) => String::from_utf8_lossy(x).to_string(),
            _ => String::from(""),
        }
    }
}
