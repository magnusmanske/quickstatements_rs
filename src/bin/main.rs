extern crate mediawiki;
extern crate wikibase;
//#[macro_use]
extern crate config;
extern crate mysql;

use quickstatements::*;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
//use config::*;
//use std::collections::HashMap;

/*
use wikibase::entity_diff::*;

use wikibase::*;

fn _einstein_categories() {
    let api = mediawiki::api::Api::new("https://en.wikipedia.org/w/api.php").unwrap();

    // Query parameters
    let params: HashMap<_, _> = vec![
        ("action", "query"),
        ("prop", "categories"),
        ("titles", "Albert Einstein"),
        ("cllimit", "500"),
    ]
    .into_iter()
    .collect();

    // Run query
    let res = api.get_query_api_json_all(&params).unwrap();

    // Parse result
    let categories: Vec<&str> = res["query"]["pages"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(_page_id, page)| {
            page["categories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["title"].as_str().unwrap())
        })
        .collect();

    dbg!(&categories);
}

fn _wikidata_edit() {
    let mut settings = Config::default();
    // File::with_name(..) is shorthand for File::from(Path::new(..))
    settings.merge(File::with_name("test.ini")).unwrap();
    let lgname = settings.get_str("user.user").unwrap();
    let lgpassword = settings.get_str("user.pass").unwrap();

    let mut api = mediawiki::api::Api::new("https://www.wikidata.org/w/api.php").unwrap();
    api.login(lgname, lgpassword).unwrap();

    let token = api.get_edit_token().unwrap();
    let params: HashMap<_, _> = vec![
        ("action", "wbeditentity"),
        ("id", "Q4115189"),
        ("data",r#"{"claims":[{"mainsnak":{"snaktype":"value","property":"P1810","datavalue":{"value":"ExampleString","type":"string"}},"type":"statement","rank":"normal"}]}"#),
        ("token", &token),
    ]
    .into_iter()
    .collect();
    let _res = api.post_query_api_json(&params).unwrap();
    //    dbg!(res["success"].as_u64().unwrap());
}

fn _wikidata_sparql() {
    let api = mediawiki::api::Api::new("https://www.wikidata.org/w/api.php").unwrap();
    let res = api.sparql_query ( "SELECT ?q ?qLabel ?fellow_id { ?q wdt:P31 wd:Q5 ; wdt:P6594 ?fellow_id . SERVICE wikibase:label { bd:serviceParam wikibase:language '[AUTO_LANGUAGE],en'. } }" ).unwrap() ;
    //println!("{}", ::serde_json::to_string_pretty(&res).unwrap());

    let mut qs = vec![];
    for b in res["results"]["bindings"].as_array().unwrap() {
        match b["q"]["value"].as_str() {
            Some(entity_url) => {
                qs.push(api.extract_entity_from_uri(entity_url).unwrap());
            }
            None => {}
        }
    }
    //println!("{}: {:?}", qs.len(), qs);
    let mut ec = wikibase::entity_container::EntityContainer::new();
    ec.load_entities(&api, &qs).unwrap();
}

fn _wikidata_item_tester() {
    let mut settings = Config::default();
    // File::with_name(..) is shorthand for File::from(Path::new(..))
    settings.merge(File::with_name("test.ini")).unwrap();
    let lgname = settings.get_str("user.user").unwrap();
    let lgpassword = settings.get_str("user.pass").unwrap();

    // Create API and log in
    let mut api = mediawiki::api::Api::new("https://www.wikidata.org/w/api.php").unwrap();
    api.login(lgname, lgpassword).unwrap();

    // Load existing item
    let q = "Q4115189"; // Sandbox item
    let mut ec = wikibase::entity_container::EntityContainer::new();
    let orig_i = ec.load_entity(&api, q).unwrap().clone();
    let mut i = orig_i.clone();

    // Alter item
    i.add_claim(Statement::new(
        "statement",
        StatementRank::Normal,
        Snak::new(
            "wikibase-item",
            "P31",
            SnakType::Value,
            Some(DataValue::new(
                DataValueType::EntityId,
                wikibase::Value::Entity(EntityValue::new(EntityType::Item, "Q12345")),
            )),
        ),
        vec![],
        vec![],
    ));

    // Compute diff between old and new item
    let mut diff_params = EntityDiffParams::none();
    diff_params.claims.add = vec!["P31".to_string()];
    let diff = EntityDiff::new(&orig_i, &i, &diff_params);
    println!("{}\n", diff.as_str().unwrap());

    // Apply diff
    let new_json =
        EntityDiff::apply_diff(&mut api, &diff, EditTarget::Entity(q.to_string())).unwrap();
    let entity_id = EntityDiff::get_entity_id(&new_json).unwrap();
    println!("=> {}", &entity_id);

    //println!("{}", ::serde_json::to_string_pretty(&new_json).unwrap());
}

fn main() {
    //_einstein_categories();
    //_wikidata_edit();
    //_wikidata_sparql();
    _wikidata_item_tester();
}*/

fn run_bot(config_arc: Arc<Mutex<QuickStatements>>) {
    println!("BOT!");
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
    let config = Arc::new(Mutex::new(QuickStatements::new_from_config_json(
        "config_rs.json",
    )));

    loop {
        run_bot(config.clone());
        thread::sleep(Duration::from_millis(1000));
    }

    /*
        if false {
            let mut settings = Config::default();
            // File::with_name(..) is shorthand for File::from(Path::new(..))
            settings.merge(File::with_name("test.ini")).unwrap();
            let lgname = settings.get_str("user.user").unwrap();
            let lgpassword = settings.get_str("user.pass").unwrap();

            // Create API and log in
            let mut api = mediawiki::api::Api::new("https://www.wikidata.org/w/api.php").unwrap();
            api.set_user_agent("Rust mediawiki crate test script");
            api.login(lgname, lgpassword).unwrap();

            let q = "Q4115189"; // Sandbox item
            let token = api.get_edit_token().unwrap();
            let params: HashMap<String, String> = vec![
                ("action".to_string(), "wbcreateclaim".to_string()),
                ("entity".to_string(), q.to_string()),
                ("property".to_string(), "P31".to_string()),
                ("snaktype".to_string(), "value".to_string()),
                (
                    "value".to_string(),
                    "{\"entity-type\":\"item\",\"id\":\"Q12345\"}".to_string(),
                ),
                ("token".to_string(), token.to_string()),
            ]
            .into_iter()
            .collect();

            let res = api.post_query_api_json(&params).unwrap();
            dbg!(&res);
        }

        let api = mediawiki::api::Api::new("https://www.wikidata.org/w/api.php").unwrap();
        println!("{}", api.user_agent_full());
    */
}
