extern crate config;
extern crate mediawiki;
extern crate mysql;
extern crate wikibase;

use quickstatements::qs_bot::QuickStatementsBot;
use quickstatements::qs_config::QuickStatements;
use quickstatements::qs_parser::QuickStatementsParser;
use std::env;
use std::io;
use std::io::prelude::*;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn run_bot(config_arc: Arc<Mutex<QuickStatements>>) {
    //println!("BOT!");
    let batch_id: i64;
    let user_id: i64;
    {
        let config = config_arc.lock().unwrap();
        let tuple = match config.get_next_batch() {
            Some(n) => n,
            None => return, // Nothing to do
        };
        batch_id = tuple.0;
        user_id = tuple.1;
    }
    thread::spawn(move || {
        println!("SPAWN: Starting batch {} for user {}", &batch_id, &user_id);
        let mut bot = QuickStatementsBot::new(config_arc.clone(), batch_id, user_id);
        match bot.start() {
            Ok(_) => while bot.run().unwrap_or(false) {},
            Err(error) => {
                println!(
                    "Error when starting bot for batch #{}: '{}'",
                    &batch_id, &error
                );
                // TODO mark this as problematic so it doesn't get run again next time?
            }
        }
    });
}

fn command_bot() {
    let config = match QuickStatements::new_from_config_json("config_rs.json") {
        Some(qs) => Arc::new(Mutex::new(qs)),
        None => panic!("Could not create QuickStatements bot from config file"),
    };

    loop {
        run_bot(config.clone());
        thread::sleep(Duration::from_millis(1000));
    }
}

fn command_parse() {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        match QuickStatementsParser::new_from_line(&line) {
            Ok(c) => println!("{}\n{:?}\n", &line, &c),
            Err(e) => println!("{}\nCOULD NOT BE PARSED: {}\n", &line, &e),
        }
    }
}

fn usage(command_name: &String) {
    println!("USAGE: {} [bot|parse]", command_name);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage(&args[0]);
        return;
    }
    match args[1].as_str() {
        "bot" => command_bot(),
        "parse" => command_parse(),
        _ => usage(&args[0]),
    }
}
