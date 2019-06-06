use mysql as my;
use regex::Regex;
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

    pub fn new_from_json(json: &Value) -> Self {
        Self {
            id: -1,
            batch_id: -1,
            num: -1,
            json: json.clone(),
            status: "".to_string(),
            message: "".to_string(),
            ts_change: "".to_string(),
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

    fn is_valid_command(&self) -> Result<(), String> {
        if !self.json.is_object() {
            return Err(format!("Not a valid command: {:?}", &self));
        }
        Ok(())
    }

    pub fn action_remove_statement(&self, statement_id: String) -> Result<Value, String> {
        Ok(json!({"action":"wbremoveclaims","claim":statement_id}))
    }

    pub fn action_remove_sitelink(
        self: &mut Self,
        item: &wikibase::Entity,
    ) -> Result<Value, String> {
        let tmp = self.json["value"].clone();
        self.json["value"] = json!("");
        let ret = self.action_set_sitelink(item);
        self.json["value"] = tmp;
        ret
    }

    pub fn action_set_sitelink(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let site = match &self.json["site"].as_str() {
            Some(s) => s.to_owned(),
            None => return Err("site not set".to_string()),
        };
        let title = match &self.json["value"].as_str() {
            Some(s) => s.to_owned(),
            None => return Err("value (title) not set".to_string()),
        };

        // Check if this same sitelink is already set
        match item.sitelinks() {
            Some(sitelinks) => {
                let title_underscores = title.replace(" ", "_");
                for sl in sitelinks {
                    if sl.site() == site && sl.title().replace(" ", "_") == title_underscores {
                        return self.already_done();
                    }
                }
            }
            None => {}
        }

        Ok(json!({
            "action":"wbsetsitelink",
            "id":self.get_prefixed_id(item.id()),
            "linksite":site,
            "linktitle":title,
        }))
    }

    fn already_done(&self) -> Result<Value, String> {
        Ok(json!({"already_done":1}))
    }

    fn action_add_statement(&self, item: &wikibase::Entity) -> Result<Value, String> {
        match self.get_statement_id(item)? {
            Some(_statement_id) => {
                //println!("Such a statement already exists as {}", &statement_id);
                return self.already_done();
            }
            None => {}
        }
        let q = item.id().to_string();
        let property = match self.json["property"].as_str() {
            Some(p) => p.to_owned(),
            None => return Err("Property not found".to_string()),
        };
        let value = serde_json::to_string(&self.json["datavalue"]["value"])
            .map_err(|e| format!("{:?}", e))?;

        Ok(json!({
            "action":"wbcreateclaim",
            "entity":self.get_prefixed_id(&q),
            "snaktype":self.get_snak_type_for_datavalue(&self.json["datavalue"])?,
            "property":property,
            "value":value
        }))
    }

    fn action_set_label(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let language = self.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = self.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        match item.label_in_locale(language) {
            Some(s) => {
                if s == text {
                    return self.already_done();
                }
            }
            None => {}
        }
        Ok(
            json!({"action":"wbsetlabel","id":self.get_prefixed_id(item.id()),"language":language,"value":text}),
        )
    }

    fn action_set_description(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let language = self.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = self.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        match item.description_in_locale(language) {
            Some(s) => {
                if s == text {
                    return self.already_done();
                }
            }
            None => {}
        }
        Ok(
            json!({"action":"wbsetdescription","id":self.get_prefixed_id(item.id()),"language":language,"value":text}),
        )
    }

    fn replace_last_item(
        &self,
        v: &mut Value,
        last_entity_id: &Option<String>,
    ) -> Result<(), String> {
        if !v.is_object() {
            return Ok(());
        }
        if last_entity_id.is_none() {
            return Ok(());
        }
        match &v["type"].as_str() {
            Some("wikibase-entityid") => {}
            _ => return Ok(()),
        }
        match &v["value"]["id"].as_str() {
            Some(id) => {
                if &QuickStatementsCommand::fix_entity_id(id.to_string()) == "LAST" {
                    let id = last_entity_id.clone().expect(
                        "QuickStatementsCommand::replace_last_item: can't clone last_entity_id",
                    );
                    v["value"]["id"] = json!(id);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Replaces LAST in the command with the last item, or fails
    /// This method is called propagateLastItem in the PHP version
    pub fn insert_last_item_into_sources_and_qualifiers(
        self: &mut Self,
        last_entity_id: &Option<String>,
    ) -> Result<(), String> {
        if last_entity_id.is_none() {
            return Ok(());
        }
        let q = last_entity_id.clone().unwrap();
        let mut json = self.json.clone();
        match self.json["item"].as_str() {
            Some("LAST") => json["item"] = json!(q),
            _ => {}
        }
        self.replace_last_item(&mut json["datavalue"], last_entity_id)?;
        self.replace_last_item(&mut json["qualifier"]["value"], &last_entity_id)?;
        match json["sources"].as_array_mut() {
            Some(arr) => {
                for mut v in arr {
                    self.replace_last_item(&mut v, last_entity_id)?
                }
            }
            None => {}
        }
        self.json = json;
        Ok(())
    }

    fn action_add_alias(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let language = self.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = self.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        Ok(
            json!({"action":"wbsetaliases","id":self.get_prefixed_id(item.id()),"language":language,"add":text}),
        )
    }

    fn action_add_qualifier(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let statement_id = match self.get_statement_id(item)? {
            Some(id) => id,
            None => {
                return Err(format!(
                    "add_qualifier: Could not get statement ID for {:?}",
                    self
                ))
            }
        };

        let qual_prop = match self.json["qualifier"]["prop"].as_str() {
            Some(p) => self.check_prop(p)?,
            None => return Err("Incomplete command parameters: prop".to_string()),
        };

        let qual_value = &self.json["qualifier"]["value"]["value"];
        if !qual_value.is_string() && !qual_value.is_object() {
            return Err("Incomplete command parameters: value.value".to_string());
        }

        Ok(json!({
            "action":"wbsetqualifier",
            "claim":statement_id,
            "property":qual_prop,
            "value":serde_json::to_string(&qual_value).map_err(|e|format!("{:?}",e))?,
            "snaktype":self.get_snak_type_for_datavalue(&self.json["qualifier"])?,
        }))
    }

    fn action_add_sources(&self, item: &wikibase::Entity) -> Result<Value, String> {
        let statement_id = match self.get_statement_id(&item)? {
            Some(id) => id,
            None => {
                return Err(format!(
                    "add_sources: Could not get statement ID for {:?}",
                    self
                ))
            }
        };

        let snaks = match &self.json["sources"].as_array() {
            Some(sources) => {
                let mut snaks = json!({});
                for source in sources.iter() {
                    //println!("SOURCE: {}", &source);
                    let prop = match source["prop"].as_str() {
                        Some(prop) => prop,
                        None => return Err("No prop value in source".to_string()),
                    };
                    let prop = self.check_prop(prop)?;
                    let snaktype = self.get_snak_type_for_datavalue(&source)?;
                    let snaktype = snaktype.to_owned();
                    let snak = match snaktype.as_str() {
                        "value" => json!({
                            "property":prop.to_owned(),
                            "snaktype":"value",
                            "datavalue":source["value"],
                        }),
                        other => json!({
                            "property":prop.to_owned(),
                            "snaktype":other,
                        }),
                    };
                    if snaks[&prop].as_array().is_none() {
                        snaks[&prop] = json!([]);
                    }
                    snaks[prop]
                        .as_array_mut()
                        .ok_or(
                            "QuickStatementsBot::add_sources snaks[prop] does not as_array_mut()"
                                .to_string(),
                        )?
                        .push(snak);
                }
                snaks
            }
            None => return Err("Incomplete command parameters: sources".to_string()),
        };

        Ok(json!({
            "action":"wbsetreference",
            "statement":statement_id,
            "snaks":serde_json::to_string(&snaks).map_err(|e|format!("{:?}",e))?,
        }))
    }

    fn action_create_entity(&self) -> Result<Value, String> {
        let data = match &self.json["data"].as_object() {
            Some(_) => match serde_json::to_string(&self.json["data"]) {
                Ok(s) => s,
                _ => "{}".to_string(),
            },
            None => "{}".to_string(),
        };
        let new_type = match self.json["type"].as_str() {
            Some(t) => t,
            None => return Err("No type set".to_string()),
        };
        Ok(json!({
            "action":"wbeditentity",
            "new":new_type,
            "data":data,
        }))
    }

    fn action_merge_entities(&self) -> Result<Value, String> {
        self.is_valid_command()?;
        let item1 = match self.json["item1"].as_str() {
            Some(t) => t,
            None => return Err("item1 not set".to_string()),
        };
        let item2 = match self.json["item2"].as_str() {
            Some(t) => t,
            None => return Err("item2 not set".to_string()),
        };

        Ok(json!({
            "action":"wbmergeitems",
            "fromid":item1,
            "toid":item2,
            "ignoreconflicts":"description"
        }))
    }

    fn add_to_entity(self: &mut Self, item: &Option<wikibase::Entity>) -> Result<Value, String> {
        let item = item
            .to_owned()
            .expect("QuickStatementsCommand::add_to_entity: item is None");
        match self.json["what"].as_str() {
            Some("label") => self.action_set_label(&item),
            Some("alias") => self.action_add_alias(&item),
            Some("description") => self.action_set_description(&item),
            Some("sitelink") => self.action_set_sitelink(&item),
            Some("statement") => self.action_add_statement(&item),
            Some("qualifier") => self.action_add_qualifier(&item),
            Some("sources") => self.action_add_sources(&item),
            other => Err(format!("Bad 'what': '{:?}'", other)),
        }
    }

    fn remove_from_entity(
        self: &mut Self,
        item: &Option<wikibase::Entity>,
    ) -> Result<Value, String> {
        let item = item
            .to_owned()
            .expect("QuickStatementsCommand::remove_from_entity: item is None");
        match self.json["what"].as_str() {
            Some("statement") => {
                let statement_id = match self.get_statement_id(&item)? {
                    Some(id) => id,
                    None => return Err("remove_statement: Statement not found".to_string()),
                };
                self.action_remove_statement(statement_id)
            }
            Some("sitelink") => self.action_remove_sitelink(&item),
            other => return Err(format!("Bad 'what': '{:?}'", other)),
        }
    }

    pub fn get_action(&self) -> Result<String, String> {
        let cj = self.json["action"].clone();
        match cj.as_str() {
            None => return Err(format!("No action in command")),
            Some("") => return Err(format!("Empty action in command")),
            Some(s) => Ok(s.to_string()),
        }
    }

    pub fn action_to_execute(
        self: &mut Self,
        main_item: &Option<wikibase::Entity>,
    ) -> Result<Value, String> {
        match self.get_action()?.as_str() {
            "add" => self.add_to_entity(main_item),
            "create" => self.action_create_entity(),
            "merge" => self.action_merge_entities(),
            "remove" => self.remove_from_entity(main_item),
            other => Err(format!("Unknown action '{}'", &other)),
        }
    }

    fn is_same_datavalue(&self, dv1: &wikibase::DataValue, dv2: &Value) -> Option<bool> {
        lazy_static! {
            static ref RE_TIME: Regex = Regex::new("^(?P<a>[+-]{0,1})0*(?P<b>.+)$")
                .expect("QuickStatementsCommand::is_same_datavalue:RE_TIME does not compile");
        }

        if dv1.value_type().string_value() != dv2["type"].as_str()? {
            return Some(false);
        }

        let v2 = &dv2["value"];
        match dv1.value() {
            wikibase::Value::Coordinate(v) => Some(
                v.globe() == v2["globe"].as_str()?
                    && *v.latitude() == v2["latitude"].as_f64()?
                    && *v.longitude() == v2["longitude"].as_f64()?,
            ),
            wikibase::Value::MonoLingual(v) => {
                Some(v.language() == v2["language"].as_str()? && v.text() == v2["text"].as_str()?)
            }
            wikibase::Value::Entity(v) => Some(v.id() == v2["id"].as_str()?),
            wikibase::Value::Quantity(v) => {
                Some(*v.amount() == v2["amount"].as_str()?.parse::<f64>().ok()?)
            }
            wikibase::Value::StringValue(v) => Some(v.to_string() == v2.as_str()?),
            wikibase::Value::Time(v) => {
                let t1 = RE_TIME.replace_all(v.time(), "$a$b");
                let t2 = RE_TIME.replace_all(v2["time"].as_str()?, "$a$b");
                Some(v.calendarmodel() == v2["calendarmodel"].as_str()? && t1 == t2)
            }
        }
    }

    fn get_prefixed_id(&self, s: &str) -> String {
        s.to_string() // TODO necessary?
    }

    fn get_snak_type_for_datavalue(&self, dv: &Value) -> Result<String, String> {
        if dv["value"].as_object().is_some() {
            return Ok("value".to_string());
        }
        let ret = match &dv["value"].as_str() {
            Some("novalue") => "novalue",
            Some("somevalue") => "somevalue",
            Some(_) => "value",
            None => return Err(format!("Cannot determine snak type: {}", dv)),
        };
        Ok(ret.to_string())
    }

    fn get_statement_id(&self, item: &wikibase::Entity) -> Result<Option<String>, String> {
        let property = match self.json["property"].as_str() {
            Some(p) => p,
            None => {
                return Err(
                    "QuickStatementsCommand::get_statement_id: Property expected but not set"
                        .to_string(),
                )
            }
        };

        for claim in item.claims() {
            if claim.main_snak().property() != property {
                continue;
            }
            let dv = match claim.main_snak().data_value() {
                Some(dv) => dv,
                None => continue,
            };
            //println!("!!{:?} : {:?}", &dv, &datavalue);
            match self.is_same_datavalue(&dv, &self.json["datavalue"]) {
                Some(b) => {
                    if b {
                        let id = claim
                            .id()
                            .ok_or(format!(
                                "QuickStatementsCommand::get_statement_id batch #{} command {:?}",
                                &self.batch_id, &self
                            ))?
                            .to_string();
                        //println!("Using statement ID '{}'", &id);
                        return Ok(Some(id));
                    }
                }
                None => continue,
            }
        }
        Ok(None)
    }

    fn check_prop(&self, s: &str) -> Result<String, String> {
        lazy_static! {
            static ref RE_PROP: Regex = Regex::new(r#"^P\d+$"#)
                .expect("QuickStatementsBot::check_prop:RE_PROP does not compile");
        }
        match RE_PROP.is_match(s) {
            true => Ok(s.to_string()),
            false => Err(format!("'{}' is not a property", &s)),
        }
    }

    pub fn get_entity_id_option(&self, v: &Value) -> Option<String> {
        match v.as_str() {
            Some(s) => Some(QuickStatementsCommand::fix_entity_id(s.to_string())),
            None => None,
        }
    }

    pub fn fix_entity_id(id: String) -> String {
        id.trim().to_uppercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_test_item() -> wikibase::Entity {
        wikibase::Entity::new_item(
            "Q12345".to_string(),
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            false,
        )
    }

    #[test]
    fn rowvalue_as_i64() {
        assert_eq!(
            QuickStatementsCommand::rowvalue_as_i64(&my::Value::Int(12345)),
            12345
        );
    }

    #[test]
    fn check_prop() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(c.check_prop("P12345"), Ok("P12345".to_string()));
        assert_eq!(
            c.check_prop("xP12345"),
            Err("'xP12345' is not a property".to_string())
        );
    }

    #[test]
    fn get_entity_id_option() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(
            c.get_entity_id_option(&json!(" Q12345 ")),
            Some("Q12345".to_string())
        );
        assert_eq!(c.get_entity_id_option(&json!({})), None);
    }

    #[test]
    fn fix_entity_id() {
        assert_eq!(
            QuickStatementsCommand::fix_entity_id(" q12345  ".to_string()),
            "Q12345".to_string()
        );
    }

    #[test]
    fn action_remove_statement() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(
            c.action_remove_statement("dummy_statement_id".to_string()),
            Ok(json!({"action":"wbremoveclaims","claim":"dummy_statement_id"}))
        );
    }

    #[test]
    fn already_done() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(c.already_done(), Ok(json!({"already_done":1})));
    }

    #[test]
    fn action_set_sitelink() {
        let c =
            QuickStatementsCommand::new_from_json(&json!({"site":"enwiki","value":"Jimbo_Wales"}));
        assert_eq!(
            c.action_set_sitelink(&empty_test_item()),
            Ok(json!({
                "action":"wbsetsitelink",
                "id":"Q12345",
                "linksite":"enwiki",
                "linktitle":"Jimbo_Wales",
            }))
        );
    }

    #[test]
    fn action_remove_sitelink() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({"site":"enwiki"}));
        let mut item = empty_test_item();
        item.set_sitelink(wikibase::SiteLink::new("enwiki", "Jimbo_Wales", vec![]));
        assert_eq!(
            c.action_remove_sitelink(&item),
            Ok(json!({
                "action":"wbsetsitelink",
                "id":"Q12345",
                "linksite":"enwiki",
                "linktitle":"",
            }))
        );
    }

    #[test]
    fn action_set_label() {
        let c =
            QuickStatementsCommand::new_from_json(&json!({"language":"it","value":"Dummy text"}));
        assert_eq!(
            c.action_set_label(&empty_test_item()),
            Ok(json!({
                "action":"wbsetlabel",
                "id":"Q12345",
                "language":"it",
                "value":"Dummy text",
            }))
        );
    }

    #[test]
    fn action_set_description() {
        let c =
            QuickStatementsCommand::new_from_json(&json!({"language":"it","value":"Dummy text"}));
        assert_eq!(
            c.action_set_description(&empty_test_item()),
            Ok(json!({
                "action":"wbsetdescription",
                "id":"Q12345",
                "language":"it",
                "value":"Dummy text",
            }))
        );
    }

    #[test]
    fn action_add_alias() {
        let c =
            QuickStatementsCommand::new_from_json(&json!({"language":"it","value":"Dummy text"}));
        assert_eq!(
            c.action_add_alias(&empty_test_item()),
            Ok(json!({
                "action":"wbsetaliases",
                "id":"Q12345",
                "language":"it",
                "add":"Dummy text",
            }))
        );
    }

    #[test]
    fn action_create_entity_without_data() {
        let c = QuickStatementsCommand::new_from_json(&json!({"type":"item"}));
        assert_eq!(
            c.action_create_entity(),
            Ok(json!({"action":"wbeditentity","new":"item","data":"{}"}))
        );
    }

    #[test]
    fn action_create_entity_with_data() {
        let c = QuickStatementsCommand::new_from_json(&json!({"type":"item","data":{"k":"v"}}));
        assert_eq!(
            c.action_create_entity(),
            Ok(json!({"action":"wbeditentity","new":"item","data":"{\"k\":\"v\"}"}))
        );
    }

    #[test]
    fn action_merge_entities() {
        let c = QuickStatementsCommand::new_from_json(&json!({"item1":"Q123","item2":"Q456"}));
        assert_eq!(
            c.action_merge_entities(),
            Ok(json!({
                "action":"wbmergeitems",
                "fromid":"Q123",
                "toid":"Q456",
                "ignoreconflicts":"description"
            }))
        );
    }

    #[test]
    fn is_same_datavalue_coordinates() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let globe = "dummy_globe".to_string();
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::GlobeCoordinate,
                    wikibase::Value::Coordinate(wikibase::Coordinate::new(
                        None,
                        globe.clone(),
                        0.123,
                        -0.456,
                        None
                    ))
                ),
                &json!({"type":"globecoordinate","value":{"globe":globe,"latitude":0.123,"longitude":-0.456}})
            )
            .unwrap());
    }

    #[test]
    fn is_same_datavalue_monolingualtext() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::MonoLingualText,
                    wikibase::Value::MonoLingual(wikibase::MonoLingualText::new(
                        "dummy text",
                        "es"
                    ))
                ),
                &json!({"type":"monolingualtext","value":{"language":"es","text":"dummy text"}})
            )
            .unwrap());
    }

    #[test]
    fn is_same_datavalue_entity() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::EntityId,
                    wikibase::Value::Entity(wikibase::EntityValue::new(
                        wikibase::EntityType::Item,
                        "Q12345"
                    ))
                ),
                &json!({"type":"wikibase-entityid","value":{"id":"Q12345"}})
            )
            .unwrap());
    }

    #[test]
    fn is_same_datavalue_string() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::StringType,
                    wikibase::Value::StringValue("dummy string")
                ),
                &json!({"type":"string","value":"dummy string"})
            )
            .unwrap());
    }

    /*
        fn is_same_datavalue(&self, dv1: &wikibase::DataValue, dv2: &Value) -> Option<bool> {
            lazy_static! {
                static ref RE_TIME: Regex = Regex::new("^(?P<a>[+-]{0,1})0*(?P<b>.+)$")
                    .expect("QuickStatementsCommand::is_same_datavalue:RE_TIME does not compile");
            }

            if dv1.value_type().string_value() != dv2["type"].as_str()? {
                return Some(false);
            }

            let v2 = &dv2["value"];
            match dv1.value() {
                wikibase::Value::Entity(v) => Some(v.id() == v2["id"].as_str()?),
                wikibase::Value::Quantity(v) => {
                    Some(*v.amount() == v2["amount"].as_str()?.parse::<f64>().ok()?)
                }
                wikibase::Value::StringValue(v) => Some(v.to_string() == v2.as_str()?),
                wikibase::Value::Time(v) => {
                    let t1 = RE_TIME.replace_all(v.time(), "$a$b");
                    let t2 = RE_TIME.replace_all(v2["time"].as_str()?, "$a$b");
                    Some(v.calendarmodel() == v2["calendarmodel"].as_str()? && t1 == t2)
                }
            }
        }

    */
    // TODO
    // action_add_statement
    // action_add_qualifier
    // action_add_sources
}
