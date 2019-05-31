use mysql as my;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct QuickStatementsCommand {
    id: i64,
    batch_id: i64,
    num: i64,
    json: Value,
    status: String,
    message: String,
    ts_change: String,
}

impl QuickStatementsCommand {
    pub fn new_from_row(row: my::Row) -> Self {
        Self {
            id: QuickStatementsCommand::rowvalue_as_i64(&row["id"]),
            batch_id: QuickStatementsCommand::rowvalue_as_i64(&row["batch_id"]),
            num: QuickStatementsCommand::rowvalue_as_i64(&row["num"]),
            json: match &row["json"] {
                my::Value::Bytes(x) => serde_json::from_str(&String::from_utf8_lossy(x)).unwrap(),
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

#[derive(Debug, Clone)]
pub struct QuickStatements {
    params: Value,
    pool: Option<my::Pool>,
    running_batch_ids: HashSet<i64>,
}

impl QuickStatements {
    pub fn new_from_config_json(filename: &str) -> Self {
        let file = File::open(filename).unwrap();
        let params: Value = serde_json::from_reader(file).unwrap();

        let mut ret = Self {
            params: params.clone(),
            pool: None,
            running_batch_ids: HashSet::new(),
        };
        ret.create_mysql_pool();
        ret
    }

    fn create_mysql_pool(&mut self) {
        // ssh magnus@tools-login.wmflabs.org -L 3307:tools-db:3306 -N
        if !self.params["mysql"].is_object() {
            return;
        }
        let mut builder = my::OptsBuilder::new();
        //println!("{}", &self.params);
        builder
            .ip_or_hostname(self.params["mysql"]["host"].as_str())
            .db_name(self.params["mysql"]["schema"].as_str())
            .user(self.params["mysql"]["user"].as_str())
            .pass(self.params["mysql"]["pass"].as_str());
        match self.params["mysql"]["port"].as_u64() {
            Some(port) => {
                builder.tcp_port(port as u16);
            }
            None => {}
        }

        self.pool = match my::Pool::new(builder) {
            Ok(pool) => Some(pool),
            _ => None,
        }
    }

    pub fn get_next_batch(&self) -> Option<i64> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        for row in pool
            .prep_exec(
                r#"SELECT * FROM batch WHERE `status` IN ('TEST') ORDER BY `ts_last_change`"#, // 'INIT','RUN' TESTING
                (),
            )
            .unwrap()
        {
            let row = row.unwrap();
            let id = match &row["id"] {
                my::Value::Int(x) => *x as i64,
                _ => continue,
            };
            if self.running_batch_ids.contains(&id) {
                continue;
            }
            return Some(id);
            /*
            let status = match &row["status"] {
                my::Value::Bytes(x) => String::from_utf8_lossy(x),
                _ => continue,
            };
            println!("{}:{}", &id, &status);
            */
        }
        None
    }

    pub fn set_batch_running(&mut self, batch_id: i64) {
        println!("set_batch_running: Starting batch #{}", batch_id);
        self.running_batch_ids.insert(batch_id);
    }

    pub fn set_batch_finished(&mut self, batch_id: i64) {
        println!("set_batch_finished: Batch #{}", batch_id);
        // TODO update batch status/time
        self.running_batch_ids.remove(&batch_id);
    }

    pub fn get_next_command(&mut self, batch_id: i64) -> Option<QuickStatementsCommand> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let sql =
            r#"SELECT * FROM command WHERE batch_id=? AND status IN ('INIT') ORDER BY num LIMIT 1"#;
        for row in pool.prep_exec(sql, (my::Value::Int(batch_id),)).unwrap() {
            let row = row.unwrap();
            return Some(QuickStatementsCommand::new_from_row(row));
        }
        None
    }

    pub fn set_command_status(
        self: &mut Self,
        command_id: i64,
        new_status: &str,
        new_message: Option<String>,
    ) {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => panic!("set_command_status: MySQL pool not available"),
        };
        let pe = match new_message {
            Some(message) => pool.prep_exec(
                r#"UPDATE command SET status=?,message=? WHERE id=?"#,
                (
                    my::Value::from(new_status),
                    my::Value::from(message),
                    my::Value::from(command_id),
                ),
            ),
            None => pool.prep_exec(
                r#"UPDATE command SET status=? WHERE id=?"#,
                (my::Value::from(new_status), my::Value::from(command_id)),
            ),
        };
        pe.unwrap();
    }
}

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: i64,
    config: Arc<Mutex<QuickStatements>>,
    last_entity_id: Option<String>,
    current_entity_id: Option<String>,
    current_property_id: Option<String>,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<Mutex<QuickStatements>>, batch_id: i64) -> Self {
        Self {
            batch_id: batch_id,
            config: config.clone(),
            last_entity_id: None,
            current_entity_id: None,
            current_property_id: None,
        }
    }

    pub fn start(self: &mut Self) {
        let mut config = self.config.lock().unwrap();
        config.set_batch_running(self.batch_id);
    }

    pub fn run(self: &mut Self) -> bool {
        println!("Batch #{}: doing stuff", self.batch_id);
        match self.get_next_command() {
            Some(mut command) => {
                self.execute_command(&mut command);
                true
            }
            None => {
                let mut config = self.config.lock().unwrap();
                config.set_batch_finished(self.batch_id);
                false
            }
        }
    }

    fn get_next_command(&self) -> Option<QuickStatementsCommand> {
        let mut config = self.config.lock().unwrap();
        config.get_next_command(self.batch_id)
    }

    fn create_new_entity(self: &mut Self, _command: &mut QuickStatementsCommand) {
        // TODO
    }

    fn merge_entities(self: &mut Self, _command: &mut QuickStatementsCommand) {
        // TODO
    }

    fn add_to_entity(self: &mut Self, command: &mut QuickStatementsCommand) {
        self.load_command_items(command);
        if self.current_entity_id.is_none() {
            return self.set_command_status(
                "ERROR",
                Some("No (last) item available".to_string()),
                command,
            );
        }
        println!(
            "{:?}/{:?}",
            &self.current_entity_id, &self.current_property_id
        );
        println!("{}", &command.json);
        panic!("OK");
        // TODO
    }

    fn remove_from_entity(self: &mut Self, command: &mut QuickStatementsCommand) {
        self.load_command_items(command);
        if self.current_entity_id.is_none() {
            return self.set_command_status(
                "ERROR",
                Some("No (last) item available".to_string()),
                command,
            );
        }
        // TODO
    }

    fn load_command_items(self: &mut Self, command: &mut QuickStatementsCommand) {
        self.current_property_id = None;
        /*
        self.current_entity_id = match command.json["item"].as_str() {
            Some(id) => Some(self.fix_entity_id(id.to_string())),
            None => self.last_entity_id.clone(),
        };
        */
    }

    fn fix_entity_id(&self, id: String) -> String {
        id.trim().to_uppercase()
    }

    fn execute_command(self: &mut Self, command: &mut QuickStatementsCommand) {
        self.set_command_status("RUN", None, command);
        // TODO set status to RUN
        self.current_property_id = None;
        self.current_entity_id = None;

        // TODO
        // $summary = "[[:toollabs:quickstatements/#/batch/{$batch_id}|batch #{$batch_id}]] by [[User:{$this->user_name}|]]" ;
        // if ( !isset($cmd->json->summary) ) $cmd->summary = $summary ; else $cmd->summary .= '; ' . $summary ;

        match command.json["action"].as_str().unwrap() {
            "create" => self.create_new_entity(command),
            "merge" => self.merge_entities(command),
            "add" => self.add_to_entity(command),
            "remove" => self.remove_from_entity(command),
            other => {
                println!(
                    "Batch {} command {} (ID {}): Unknown action '{}'",
                    command.batch_id, command.num, command.id, &other
                );
                self.set_command_status(
                    "ERROR",
                    Some("Incomplete or unknown command".to_string()),
                    command,
                )
            }
        }
    }

    fn set_command_status(
        self: &mut Self,
        status: &str,
        message: Option<String>,
        command: &mut QuickStatementsCommand,
    ) {
        if status == "DONE" {
            if self.current_entity_id.is_some() {
                self.last_entity_id = self.current_entity_id.clone();
            }
        }

        let mut config = self.config.lock().unwrap();
        config.set_command_status(command.id, status, message);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
