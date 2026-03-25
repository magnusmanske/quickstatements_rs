use serde_json::json;

use clap::Parser;
use log::{error, info};
use quickstatements::qs_bot::QuickStatementsBot;
use quickstatements::qs_command::QuickStatementsCommand;
use quickstatements::qs_config::QuickStatements;
use quickstatements::qs_parser::QuickStatementsParser;
use quickstatements::qs_server;
use std::io;
use std::io::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SLEEP_BETWEEN_BOT_RUNS_MS: u64 = 500;
const MAX_INACTIVITY_BEFORE_SEPPUKU_SEC: u64 = 60;

async fn start_batch(config: Arc<QuickStatements>, batch_id: i64, user_id: i64) {
    println!("Starting batch {} for user {}", &batch_id, &user_id);
    let mut bot = QuickStatementsBot::new(config.clone(), Some(batch_id), user_id);

    match bot.start().await {
        Ok(_) => {
            tokio::spawn(async move { while bot.run().await.unwrap_or(false) {} });
        }
        Err(error) => {
            println!(
                "Error when starting bot for batch #{}: '{}'",
                &batch_id, &error
            );
        }
    }
}

/// Start all available batches in one go, returning the number started.
async fn run_bot(config: Arc<QuickStatements>) -> usize {
    let batches = config.get_next_batches().await;
    let count = batches.len();
    for (batch_id, user_id) in batches {
        start_batch(config.clone(), batch_id, user_id).await;
    }
    count
}

async fn command_bot(verbose: bool, config_file: &str) {
    let cpus = num_cpus::get();
    println!("{} CPUs available", cpus);
    let config = match QuickStatements::new_from_config_json(config_file) {
        Some(mut qs) => {
            qs.set_verbose(verbose);
            Arc::new(qs)
        }
        None => panic!("Could not create QuickStatements bot from config file"),
    };

    config
        .reset_all_running_batches()
        .await
        .expect("Could not reset running batches");

    let last_bot_run = Arc::new(Mutex::new(Instant::now()));
    seppuku(config.clone(), last_bot_run.clone());

    // Run bot
    loop {
        let started = run_bot(config.clone()).await;
        if started > 0 {
            *last_bot_run.lock().unwrap() = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(SLEEP_BETWEEN_BOT_RUNS_MS)).await;
    }
}

/// Seppuku if no activity for a while
fn seppuku(config: Arc<QuickStatements>, last_bot_run: Arc<Mutex<Instant>>) {
    tokio::spawn(async move {
        loop {
            let last = *last_bot_run.lock().unwrap();
            if last.elapsed().as_secs() > MAX_INACTIVITY_BEFORE_SEPPUKU_SEC
                && config.get_next_batch().await.is_some()
            {
                println!("Commiting seppuku");
                std::process::exit(0);
            }
            tokio::time::sleep(Duration::from_secs(MAX_INACTIVITY_BEFORE_SEPPUKU_SEC)).await;
        }
    });
}

async fn get_php_commands(
    api: &wikibase::mediawiki::api::Api,
    lines: String,
) -> Vec<serde_json::Value> {
    let params = api.params_into(&[
        ("action", "import"),
        ("compress", "1"),
        ("format", "v1"),
        ("persistent", "0"),
        ("data", lines.as_str()),
    ]);
    let j = api
        .query_raw(
            "https://tools.wmflabs.org/quickstatements/api.php",
            &params,
            "POST",
        )
        .await
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&j).unwrap();
    //println!("{}", &j);
    match j["data"]["commands"].as_array() {
        Some(commands) => commands.to_vec(),
        None => vec![],
    }
}

async fn get_commands(
    api: &wikibase::mediawiki::api::Api,
    lines: &[String],
) -> Vec<QuickStatementsParser> {
    let mut ret: Vec<QuickStatementsParser> = vec![];
    for line in lines {
        match QuickStatementsParser::new_from_line(line, Some(api)).await {
            Ok(c) => {
                ret.push(c);
            }
            Err(e) => error!("\n{}\nCOULD NOT BE PARSED: {}\n", &line, &e),
        }
    }
    ret
}

async fn command_parse() {
    let stdin = io::stdin();
    let api = wikibase::mediawiki::api::Api::new("https://commons.wikimedia.org/w/api.php")
        .await
        .unwrap();
    let mut lines = vec![];
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        lines.push(line);
    }
    let mut commands = get_commands(&api, &lines).await;
    QuickStatementsParser::compress(&mut commands);
    let commands_json: Vec<serde_json::Value> =
        commands.iter().flat_map(|c| c.to_json().unwrap()).collect();
    let commands_json = json!({"data":{"commands":json!(commands_json)},"status":"OK"});
    println!("{}", commands_json);
}

async fn command_validate() {
    let stdin = io::stdin();
    let api = wikibase::mediawiki::api::Api::new("https://commons.wikimedia.org/w/api.php")
        .await
        .unwrap();
    let mut lines = vec![];
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        lines.push(line);
    }
    let php_commands = get_php_commands(&api, lines.join("\n")).await;
    let mut commands = get_commands(&api, &lines).await;
    QuickStatementsParser::compress(&mut commands);
    let commands_json: Vec<serde_json::Value> =
        commands.iter().flat_map(|c| c.to_json().unwrap()).collect();

    if commands_json == php_commands {
        info!("Perfect!");
    //println!("{}", json!(commands_json));
    } else {
        error!("Mismatch");
        println!("\n{}\n", json!(commands_json));
        println!("{}", json!(php_commands));
    }
}

async fn command_debug_command(config_file: &str, command_id: i64) {
    let config = match QuickStatements::new_from_config_json(config_file) {
        Some(qs) => Arc::new(qs),
        None => panic!("Could not create QuickStatements from config file"),
    };

    // Load the command from DB
    let mut command = config
        .get_command_by_id(command_id)
        .await
        .unwrap_or_else(|| panic!("Command #{} not found in database", command_id));

    println!("Command #{}:", command_id);
    println!("  Batch:  {}", command.batch_id);
    println!("  Num:    {}", command.num);
    println!("  Status: {}", command.status);
    println!("  JSON:   {}", serde_json::to_string_pretty(&command.json).unwrap());
    println!();

    // Get the API URL for this batch's site
    let api_url = config
        .get_api_url(command.batch_id)
        .await
        .unwrap_or_else(|| panic!("No API URL for batch #{}", command.batch_id));

    // Set up a bot with API + auth
    let mut mw_api = wikibase::mediawiki::api::Api::new(api_url)
        .await
        .unwrap_or_else(|e| panic!("Could not create API: {:?}", e));
    config.set_bot_api_auth(&mut mw_api, command.batch_id).await;

    // Load LAST state from the batch
    let last_state = config.get_last_state_from_batch(command.batch_id).await;

    let mut bot = QuickStatementsBot::new(config.clone(), Some(command.batch_id), 0);
    bot.set_mw_api(mw_api);
    bot.set_last_state(last_state);

    match bot.debug_command(&mut command).await {
        Ok((params, response)) => {
            // Print request params
            let params_json: serde_json::Value = params
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            println!("API parameters:");
            println!("{}", serde_json::to_string_pretty(&params_json).unwrap());

            // Print response
            println!();
            println!("API response:");
            println!("{}", serde_json::to_string_pretty(&response).unwrap());

            // Highlight key fields
            if let Some(code) = response["error"]["code"].as_str() {
                println!();
                println!("Error code: {}", code);
                if let Some(info) = response["error"]["info"].as_str() {
                    println!("Error info: {}", info);
                }
            } else if let Some(success) = response["success"].as_i64() {
                println!();
                println!("Success: {}", success);
            }
        }
        Err(e) => {
            eprintln!("Error preparing command: {}", e);
            std::process::exit(1);
        }
    }
}

async fn command_server(config_file: &str, port: u16) {
    let config = match QuickStatements::new_from_config_json(config_file) {
        Some(qs) => Arc::new(qs),
        None => panic!("Could not create QuickStatements from config file"),
    };

    let app = qs_server::build_router(config);
    let addr = format!("0.0.0.0:{}", port);
    println!("QuickStatements server listening on http://{}", &addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn command_run(site: &str) {
    // Initialize config
    let config = match QuickStatements::new_from_config_json("config_rs.json") {
        Some(qs) => Arc::new(qs),
        None => panic!("Could not create QuickStatements bot from config file"),
    };

    let api_url = match config.get_api_for_site(site) {
        Some(url) => url,
        None => panic!("Could not get API URL for site '{}'", site),
    }
    .to_owned();

    println!("{}: {}", site, &api_url);

    let mut bot = QuickStatementsBot::new(config.clone(), None, 0);

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let command_string = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if command_string.is_empty() {
            continue;
        }

        // Parse command
        let json_commands = match QuickStatementsParser::new_from_line(&command_string, None).await
        {
            Ok(c) => c.to_json().unwrap(),
            Err(e) => {
                println!("{}\nCOULD NOT BE PARSED: {}\n", &command_string, &e);
                return;
            }
        };

        json_commands.iter().for_each(|c| {
            println!("{}", ::serde_json::to_string_pretty(c).unwrap());
        });

        for json_command in json_commands {
            // Generate command
            let mut command = QuickStatementsCommand::new_from_json(&json_command);

            // Run command
            bot.set_mw_api(wikibase::mediawiki::api::Api::new(&api_url).await.unwrap());
            //bot.set_mw_api(wikibase::mediawiki::api::Api::new("https://test.wikidata.org/w/api.php").unwrap());
            bot.execute_command(&mut command).await.unwrap();
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = "Runs QuickStatement bot or command line operations"
)]
struct Args {
    /// Sets a site for RUN command
    #[arg(short, long, default_value_t=format!("wikidata"))]
    site: String,

    #[arg(short, long)]
    verbose: bool,

    /// Command [bot|parse|validate|run|server]
    #[arg(long)]
    command: String,

    /// Configuration file (JSON)
    #[arg(long, default_value_t=format!("config_rs.json"))]
    config_file: String,

    /// Port for the web server (server command)
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Command ID for debug_command
    #[arg(long)]
    id: Option<i64>,
}

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    let args = Args::parse();
    match args.command.as_str() {
        "bot" => command_bot(args.verbose, &args.config_file).await,
        "parse" => command_parse().await,
        "validate" => command_validate().await,
        "run" => command_run(&args.site).await,
        "server" => command_server(&args.config_file, args.port).await,
        "debug_command" => {
            let id = args.id.expect("--id is required for debug_command");
            command_debug_command(&args.config_file, id).await;
        }
        x => panic!("Not a valid command: {}", x),
    }
}

/*
ssh magnus@login.toolforge.org -L 3308:tools-db:3306 -N &
cd ~/rust/quickstatements_rs/ && git pull && ./build.sh && \
toolforge jobs delete bot ; rm ~/bot.out ~/bot.err ; \
toolforge jobs run --image golang1.11 --continuous --mem 1500Mi --command '/data/project/quickstatements/rust/quickstatements_rs/target/release/main --command bot' bot

clear ; toolforge-jobs list ; tail ~/bot.err ~/bot.out
*/
