extern crate config;
extern crate mediawiki;
extern crate mysql;
extern crate wikibase;

use quickstatements::qs_bot::QuickStatementsBot;
use quickstatements::qs_config::QuickStatements;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn run_bot(config_arc: Arc<Mutex<QuickStatements>>) {
    //println!("BOT!");
    let batch_id;
    {
        let config = config_arc.lock().unwrap();
        batch_id = match config.get_next_batch() {
            Some(id) => id as i64,
            None => return, // Nothing to do
        };
    }
    thread::spawn(move || {
        println!("SPAWN: Starting batch {}", &batch_id);
        let mut bot = QuickStatementsBot::new(config_arc.clone(), batch_id);
        bot.start();
        while bot.run() {}
    });
}

fn main() {
    let config = match QuickStatements::new_from_config_json("config_rs.json") {
        Some(qs) => Arc::new(Mutex::new(qs)),
        None => panic!("Could not create QuickStatements bot from config file"),
    };

    loop {
        run_bot(config.clone());
        thread::sleep(Duration::from_millis(1000));
    }
}
