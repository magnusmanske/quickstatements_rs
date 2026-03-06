use regex::Regex;
use serde_json::{json, Value};
use wikibase::*;

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
    pub fn from_row(r: &(i64, i64, i64, String, String, String, String)) -> Self {
        Self {
            id: r.0,
            batch_id: r.1,
            num: r.2,
            json: serde_json::from_str(&r.3).unwrap_or(json!({})),
            status: r.4.to_owned(),
            message: r.5.to_owned(),
            ts_change: r.6.to_owned(),
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

    fn is_valid_command(&self) -> Result<(), String> {
        if !self.json.is_object() {
            return Err(format!("Not a valid command: {:?}", &self));
        }
        Ok(())
    }

    pub fn action_remove_statement(&self, statement_id: String) -> Result<Value, String> {
        Ok(json!({"action":"wbremoveclaims","claim":statement_id}))
    }

    pub fn action_remove_sitelink(&mut self, item: &wikibase::Entity) -> Result<Value, String> {
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
        if let Some(sitelinks) = item.sitelinks() {
            let title_underscores = title.replace(' ', "_");
            for sl in sitelinks {
                if sl.site() == site && sl.title().replace(' ', "_") == title_underscores {
                    return self.already_done();
                }
            }
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
        if let Some(_statement_id) = self.get_statement_id(item)? {
            //println!("Such a statement already exists as {}", &statement_id);
            return self.already_done();
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
        if let Some(s) = item.label_in_locale(language) {
            if s == text {
                return self.already_done();
            }
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
        if let Some(s) = item.description_in_locale(language) {
            if s == text {
                return self.already_done();
            }
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
        &mut self,
        last_entity_id: &Option<String>,
    ) -> Result<(), String> {
        if last_entity_id.is_none() {
            return Ok(());
        }
        let q = last_entity_id.clone().unwrap();
        let mut json = self.json.clone();
        if let Some("LAST") = self.json["item"].as_str() {
            json["item"] = json!(q)
        }
        self.replace_last_item(&mut json["datavalue"], last_entity_id)?;
        self.replace_last_item(&mut json["qualifier"]["value"], last_entity_id)?;
        if let Some(arr) = json["sources"].as_array_mut() {
            for v in arr {
                self.replace_last_item(v, last_entity_id)?
            }
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
        let statement_id = match self.get_statement_id(item)? {
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
                    let snaktype = self.get_snak_type_for_datavalue(source)?;
                    let snak = match snaktype.as_str() {
                        "value" => json!({
                            "property":&prop,
                            "snaktype":"value",
                            "datavalue":source["value"],
                        }),
                        other => json!({
                            "property":&prop,
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

    fn add_to_entity(&mut self, item: &Option<wikibase::Entity>) -> Result<Value, String> {
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

    fn remove_from_entity(&mut self, item: &Option<wikibase::Entity>) -> Result<Value, String> {
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
            other => Err(format!("Bad 'what': '{:?}'", other)),
        }
    }

    pub fn get_action(&self) -> Result<String, String> {
        let cj = self.json["action"].clone();
        match cj.as_str() {
            None => Err("No action in command".to_string()),
            Some("") => Err("Empty action in command".to_string()),
            Some(s) => Ok(s.to_string()),
        }
    }

    pub fn action_to_execute(
        &mut self,
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
            wikibase::Value::StringValue(v) => Some(*v == v2.as_str()?),
            wikibase::Value::Time(v) => {
                let t1 = RE_TIME.replace_all(v.time(), "$a$b");
                let t2 = RE_TIME.replace_all(v2["time"].as_str()?, "$a$b");
                Some(v.calendarmodel() == v2["calendarmodel"].as_str()? && t1 == t2)
            }
            wikibase::Value::EntitySchema(es) => Some(es.id() == v2["id"].as_str()?),
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
        // Try directly by statement ID, as string
        if let Some(id) = self.json["id"].as_str() {
            return Ok(Some(id.to_string()));
        }

        // No ID given, find the property
        let property = match self.json["property"].as_str() {
            Some(p) => p,
            None => {
                return Err(
                    "QuickStatementsCommand::get_statement_id: Property expected but not set"
                        .to_string(),
                )
            }
        };

        // Find the correct value for the property
        for claim in item.claims() {
            if claim.main_snak().property() != property {
                continue;
            }
            let dv = match claim.main_snak().data_value() {
                Some(dv) => dv,
                None => continue,
            };
            //println!("!!{:?} : {:?}", &dv, &datavalue);
            match self.is_same_datavalue(dv, &self.json["datavalue"]) {
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
        v.as_str()
            .map(|s| QuickStatementsCommand::fix_entity_id(s.to_string()))
    }

    pub fn fix_entity_id(id: String) -> String {
        lazy_static! {
            static ref RE_STATEMENT_ID: Regex = Regex::new(r#"\$.*$"#)
                .expect("QuickStatementsBot::fix_entity_id:RE_STATEMENT_ID does not compile");
        }
        RE_STATEMENT_ID.replace_all(&id, "").trim().to_uppercase()
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
        assert_eq!(
            QuickStatementsCommand::fix_entity_id(" q12345$foobar  ".to_string()),
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
                    wikibase::Value::StringValue("dummy string".to_string())
                ),
                &json!({"type":"string","value":"dummy string"})
            )
            .unwrap());
    }

    #[test]
    fn is_same_datavalue_quantity() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::Quantity,
                    wikibase::Value::Quantity(wikibase::QuantityValue::new(
                        -123.45, None, "1", None
                    ))
                ),
                &json!({"type":"quantity","value":{"amount":"-123.45"}})
            )
            .unwrap());
    }

    #[test]
    fn is_same_datavalue_time() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let calendarmodel = "http://www.wikidata.org/entity/Q1985727";
        assert!(c
            .is_same_datavalue(
                &wikibase::DataValue::new(
                    wikibase::DataValueType::Time,
                    wikibase::Value::Time(wikibase::TimeValue::new(
                        0,0,calendarmodel,11,"+2019-06-06T00:00:00Z",0
                    ))
                ),
                &json!({"type":"time","value":{"time":"+0002019-06-06T00:00:00Z","calendarmodel":calendarmodel}})
            )
            .unwrap());
    }

    #[test]
    fn get_snak_type_for_datavalue() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(
            c.get_snak_type_for_datavalue(&json!({"value":{}})),
            Ok("value".to_string())
        );
        assert_eq!(
            c.get_snak_type_for_datavalue(&json!({"value":"novalue"})),
            Ok("novalue".to_string())
        );
        assert_eq!(
            c.get_snak_type_for_datavalue(&json!({"value":"somevalue"})),
            Ok("somevalue".to_string())
        );
        assert_eq!(
            c.get_snak_type_for_datavalue(&json!({"value":"foobar"})),
            Ok("value".to_string())
        );
        let dv = json!({"foo":"bar"});
        assert_eq!(
            c.get_snak_type_for_datavalue(&dv),
            Err(format!("Cannot determine snak type: {}", &dv))
        );
    }

    #[test]
    fn from_row() {
        let row = (
            1_i64,
            2_i64,
            3_i64,
            r#"{"action":"add"}"#.to_string(),
            "INIT".to_string(),
            "some message".to_string(),
            "20230101120000".to_string(),
        );
        let cmd = QuickStatementsCommand::from_row(&row);
        assert_eq!(cmd.id, 1);
        assert_eq!(cmd.batch_id, 2);
        assert_eq!(cmd.num, 3);
        assert_eq!(cmd.json["action"], "add");
        assert_eq!(cmd.status, "INIT");
        assert_eq!(cmd.message, "some message");
        assert_eq!(cmd.ts_change, "20230101120000");
    }

    #[test]
    fn from_row_invalid_json() {
        let row = (
            1_i64,
            2_i64,
            3_i64,
            "not valid json".to_string(),
            "INIT".to_string(),
            "".to_string(),
            "".to_string(),
        );
        let cmd = QuickStatementsCommand::from_row(&row);
        assert_eq!(cmd.json, json!({}));
    }

    #[test]
    fn new_from_json_defaults() {
        let c = QuickStatementsCommand::new_from_json(&json!({"action":"add"}));
        assert_eq!(c.id, -1);
        assert_eq!(c.batch_id, -1);
        assert_eq!(c.num, -1);
        assert_eq!(c.status, "");
        assert_eq!(c.message, "");
        assert_eq!(c.ts_change, "");
        assert_eq!(c.json["action"], "add");
    }

    #[test]
    fn is_valid_command_ok() {
        let c = QuickStatementsCommand::new_from_json(&json!({"action":"add"}));
        assert!(c.is_valid_command().is_ok());
    }

    #[test]
    fn is_valid_command_not_object() {
        let c = QuickStatementsCommand::new_from_json(&json!("string"));
        assert!(c.is_valid_command().is_err());
    }

    #[test]
    fn get_action_ok() {
        let c = QuickStatementsCommand::new_from_json(&json!({"action":"add"}));
        assert_eq!(c.get_action(), Ok("add".to_string()));
    }

    #[test]
    fn get_action_missing() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c.get_action().is_err());
    }

    #[test]
    fn get_action_empty() {
        let c = QuickStatementsCommand::new_from_json(&json!({"action":""}));
        assert_eq!(c.get_action(), Err("Empty action in command".to_string()));
    }

    #[test]
    fn action_to_execute_create() {
        let mut c =
            QuickStatementsCommand::new_from_json(&json!({"action":"create","type":"item"}));
        let result = c.action_to_execute(&None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["action"], "wbeditentity");
    }

    #[test]
    fn action_to_execute_merge() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"action":"merge","item1":"Q1","item2":"Q2"}),
        );
        let result = c.action_to_execute(&None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["action"], "wbmergeitems");
    }

    #[test]
    fn action_to_execute_unknown_action() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({"action":"unknown_action"}));
        let result = c.action_to_execute(&None);
        assert!(result.is_err());
    }

    #[test]
    fn action_to_execute_add_label() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"label","language":"en","value":"Test Label"}),
        );
        let item = empty_test_item();
        let result = c.action_to_execute(&Some(item));
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r["action"], "wbsetlabel");
        assert_eq!(r["value"], "Test Label");
    }

    #[test]
    fn action_to_execute_add_description() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"description","language":"en","value":"Test Desc"}),
        );
        let item = empty_test_item();
        let result = c.action_to_execute(&Some(item));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["action"], "wbsetdescription");
    }

    #[test]
    fn action_to_execute_add_alias() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"alias","language":"en","value":"Test Alias"}),
        );
        let item = empty_test_item();
        let result = c.action_to_execute(&Some(item));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["action"], "wbsetaliases");
    }

    #[test]
    fn action_to_execute_add_sitelink() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"sitelink","site":"enwiki","value":"Test Page"}),
        );
        let item = empty_test_item();
        let result = c.action_to_execute(&Some(item));
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["action"], "wbsetsitelink");
    }

    #[test]
    fn action_to_execute_bad_what() {
        let mut c =
            QuickStatementsCommand::new_from_json(&json!({"action":"add","what":"nonsense"}));
        let item = empty_test_item();
        let result = c.action_to_execute(&Some(item));
        assert!(result.is_err());
    }

    #[test]
    fn action_set_label_already_done() {
        let mut item = empty_test_item();
        item.set_label(wikibase::LocaleString::new("en", "Existing Label"));
        let c = QuickStatementsCommand::new_from_json(
            &json!({"language":"en","value":"Existing Label"}),
        );
        assert_eq!(c.action_set_label(&item), Ok(json!({"already_done":1})));
    }

    #[test]
    fn action_set_label_missing_language() {
        let c = QuickStatementsCommand::new_from_json(&json!({"value":"Test"}));
        assert!(c.action_set_label(&empty_test_item()).is_err());
    }

    #[test]
    fn action_set_label_missing_value() {
        let c = QuickStatementsCommand::new_from_json(&json!({"language":"en"}));
        assert!(c.action_set_label(&empty_test_item()).is_err());
    }

    #[test]
    fn action_set_description_already_done() {
        let mut item = empty_test_item();
        item.set_description(wikibase::LocaleString::new("en", "Existing Desc"));
        let c = QuickStatementsCommand::new_from_json(
            &json!({"language":"en","value":"Existing Desc"}),
        );
        assert_eq!(
            c.action_set_description(&item),
            Ok(json!({"already_done":1}))
        );
    }

    #[test]
    fn action_set_description_missing_language() {
        let c = QuickStatementsCommand::new_from_json(&json!({"value":"Test"}));
        assert!(c.action_set_description(&empty_test_item()).is_err());
    }

    #[test]
    fn action_set_description_missing_value() {
        let c = QuickStatementsCommand::new_from_json(&json!({"language":"en"}));
        assert!(c.action_set_description(&empty_test_item()).is_err());
    }

    #[test]
    fn action_set_sitelink_already_done() {
        let mut item = empty_test_item();
        item.set_sitelink(wikibase::SiteLink::new("enwiki", "Test Page", vec![]));
        let c =
            QuickStatementsCommand::new_from_json(&json!({"site":"enwiki","value":"Test Page"}));
        assert_eq!(c.action_set_sitelink(&item), Ok(json!({"already_done":1})));
    }

    #[test]
    fn action_set_sitelink_already_done_underscores() {
        let mut item = empty_test_item();
        item.set_sitelink(wikibase::SiteLink::new("enwiki", "Test Page", vec![]));
        let c =
            QuickStatementsCommand::new_from_json(&json!({"site":"enwiki","value":"Test_Page"}));
        assert_eq!(c.action_set_sitelink(&item), Ok(json!({"already_done":1})));
    }

    #[test]
    fn action_set_sitelink_missing_site() {
        let c = QuickStatementsCommand::new_from_json(&json!({"value":"Page"}));
        assert!(c.action_set_sitelink(&empty_test_item()).is_err());
    }

    #[test]
    fn action_set_sitelink_missing_value() {
        let c = QuickStatementsCommand::new_from_json(&json!({"site":"enwiki"}));
        assert!(c.action_set_sitelink(&empty_test_item()).is_err());
    }

    #[test]
    fn action_create_entity_no_type() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c.action_create_entity().is_err());
    }

    #[test]
    fn action_merge_entities_missing_item1() {
        let c = QuickStatementsCommand::new_from_json(&json!({"item2":"Q456"}));
        assert!(c.action_merge_entities().is_err());
    }

    #[test]
    fn action_merge_entities_missing_item2() {
        let c = QuickStatementsCommand::new_from_json(&json!({"item1":"Q123"}));
        assert!(c.action_merge_entities().is_err());
    }

    #[test]
    fn action_merge_entities_not_object() {
        let c = QuickStatementsCommand::new_from_json(&json!("string"));
        assert!(c.action_merge_entities().is_err());
    }

    #[test]
    fn get_snak_type_for_datavalue_object() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(
            c.get_snak_type_for_datavalue(&json!({"value":{"key":"val"}})),
            Ok("value".to_string())
        );
    }

    #[test]
    fn get_snak_type_for_datavalue_null() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let dv = json!({"value": null});
        assert!(c.get_snak_type_for_datavalue(&dv).is_err());
    }

    #[test]
    fn check_prop_valid() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(c.check_prop("P1"), Ok("P1".to_string()));
        assert_eq!(c.check_prop("P999999"), Ok("P999999".to_string()));
    }

    #[test]
    fn check_prop_invalid_lowercase() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c.check_prop("p123").is_err());
    }

    #[test]
    fn check_prop_invalid_prefix() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c.check_prop("Q123").is_err());
    }

    #[test]
    fn check_prop_invalid_no_digits() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert!(c.check_prop("P").is_err());
    }

    #[test]
    fn get_entity_id_option_none_for_non_string() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(c.get_entity_id_option(&json!(123)), None);
        assert_eq!(c.get_entity_id_option(&json!(null)), None);
        assert_eq!(c.get_entity_id_option(&json!(true)), None);
    }

    #[test]
    fn fix_entity_id_no_statement_suffix() {
        assert_eq!(
            QuickStatementsCommand::fix_entity_id("Q42".to_string()),
            "Q42"
        );
    }

    #[test]
    fn fix_entity_id_with_statement_suffix() {
        assert_eq!(
            QuickStatementsCommand::fix_entity_id("Q42$some-guid".to_string()),
            "Q42"
        );
    }

    #[test]
    fn fix_entity_id_lowercase() {
        assert_eq!(
            QuickStatementsCommand::fix_entity_id("q42".to_string()),
            "Q42"
        );
    }

    #[test]
    fn insert_last_item_none_is_noop() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"item":"Q123","datavalue":{"type":"string","value":"test"}}),
        );
        let original_json = c.json.clone();
        c.insert_last_item_into_sources_and_qualifiers(&None)
            .unwrap();
        assert_eq!(c.json, original_json);
    }

    #[test]
    fn insert_last_item_replaces_item_last() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"item":"LAST","datavalue":{"type":"string","value":"test"}}),
        );
        c.insert_last_item_into_sources_and_qualifiers(&Some("Q999".to_string()))
            .unwrap();
        assert_eq!(c.json["item"], "Q999");
    }

    #[test]
    fn insert_last_item_replaces_datavalue_last() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({
            "item":"Q123",
            "datavalue":{"type":"wikibase-entityid","value":{"id":"LAST"}}
        }));
        c.insert_last_item_into_sources_and_qualifiers(&Some("Q999".to_string()))
            .unwrap();
        assert_eq!(c.json["datavalue"]["value"]["id"], "Q999");
    }

    #[test]
    fn insert_last_item_replaces_qualifier_last() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({
            "item":"Q123",
            "qualifier":{"value":{"type":"wikibase-entityid","value":{"id":"LAST"}}}
        }));
        c.insert_last_item_into_sources_and_qualifiers(&Some("Q999".to_string()))
            .unwrap();
        assert_eq!(c.json["qualifier"]["value"]["value"]["id"], "Q999");
    }

    #[test]
    fn insert_last_item_replaces_sources_last() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({
            "item":"Q123",
            "sources":[{"type":"wikibase-entityid","value":{"id":"LAST"}}]
        }));
        c.insert_last_item_into_sources_and_qualifiers(&Some("Q999".to_string()))
            .unwrap();
        assert_eq!(c.json["sources"][0]["value"]["id"], "Q999");
    }

    #[test]
    fn insert_last_item_does_not_replace_non_last() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({
            "item":"Q123",
            "datavalue":{"type":"wikibase-entityid","value":{"id":"Q456"}}
        }));
        c.insert_last_item_into_sources_and_qualifiers(&Some("Q999".to_string()))
            .unwrap();
        assert_eq!(c.json["datavalue"]["value"]["id"], "Q456");
    }

    #[test]
    fn action_add_statement_no_property() {
        let c = QuickStatementsCommand::new_from_json(&json!({
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}}
        }));
        let result = c.action_add_statement(&empty_test_item());
        assert!(result.is_err());
    }

    #[test]
    fn action_add_statement_ok() {
        let c = QuickStatementsCommand::new_from_json(&json!({
            "property":"P31",
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}}
        }));
        let result = c.action_add_statement(&empty_test_item());
        assert!(result.is_ok());
        let r = result.unwrap();
        assert_eq!(r["action"], "wbcreateclaim");
        assert_eq!(r["property"], "P31");
        assert_eq!(r["entity"], "Q12345");
    }

    #[test]
    fn action_add_qualifier_no_statement() {
        let c = QuickStatementsCommand::new_from_json(&json!({
            "property":"P31",
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}},
            "qualifier":{"prop":"P585","value":{"type":"time","value":{"time":"+2020-01-01T00:00:00Z","calendarmodel":"http://www.wikidata.org/entity/Q1985727"}}}
        }));
        let result = c.action_add_qualifier(&empty_test_item());
        assert!(result.is_err());
    }

    #[test]
    fn action_add_sources_no_statement() {
        let c = QuickStatementsCommand::new_from_json(&json!({
            "property":"P31",
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}},
            "sources":[{"prop":"P248","value":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q36578"}}}]
        }));
        let result = c.action_add_sources(&empty_test_item());
        assert!(result.is_err());
    }

    #[test]
    fn action_remove_sitelink_restores_value() {
        let mut c = QuickStatementsCommand::new_from_json(
            &json!({"site":"enwiki","value":"Original Title"}),
        );
        let mut item = empty_test_item();
        item.set_sitelink(wikibase::SiteLink::new("enwiki", "Some Page", vec![]));
        let _ = c.action_remove_sitelink(&item);
        // The original value should be restored after the call
        assert_eq!(c.json["value"], "Original Title");
    }

    #[test]
    fn is_same_datavalue_different_types() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::StringType,
                wikibase::Value::StringValue("test".to_string()),
            ),
            &json!({"type":"quantity","value":{"amount":"42"}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_string_mismatch() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::StringType,
                wikibase::Value::StringValue("hello".to_string()),
            ),
            &json!({"type":"string","value":"world"}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_entity_mismatch() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::EntityId,
                wikibase::Value::Entity(wikibase::EntityValue::new(
                    wikibase::EntityType::Item,
                    "Q1",
                )),
            ),
            &json!({"type":"wikibase-entityid","value":{"id":"Q2"}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_quantity_mismatch() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::Quantity,
                wikibase::Value::Quantity(wikibase::QuantityValue::new(42.0, None, "1", None)),
            ),
            &json!({"type":"quantity","value":{"amount":"99"}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_time_different_calendar() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let calendarmodel = "http://www.wikidata.org/entity/Q1985727";
        let other_calendar = "http://www.wikidata.org/entity/Q1985786";
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::Time,
                wikibase::Value::Time(wikibase::TimeValue::new(
                    0,
                    0,
                    calendarmodel,
                    11,
                    "+2019-06-06T00:00:00Z",
                    0,
                )),
            ),
            &json!({"type":"time","value":{"time":"+2019-06-06T00:00:00Z","calendarmodel":other_calendar}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_coordinate_mismatch() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let globe = "http://www.wikidata.org/entity/Q2";
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::GlobeCoordinate,
                wikibase::Value::Coordinate(wikibase::Coordinate::new(
                    None,
                    globe.to_string(),
                    1.0,
                    2.0,
                    None,
                )),
            ),
            &json!({"type":"globecoordinate","value":{"globe":globe,"latitude":3.0,"longitude":4.0}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn is_same_datavalue_monolingual_mismatch() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.is_same_datavalue(
            &wikibase::DataValue::new(
                wikibase::DataValueType::MonoLingualText,
                wikibase::Value::MonoLingual(wikibase::MonoLingualText::new("hello", "en")),
            ),
            &json!({"type":"monolingualtext","value":{"language":"en","text":"world"}}),
        );
        assert_eq!(result, Some(false));
    }

    #[test]
    fn get_prefixed_id() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        assert_eq!(c.get_prefixed_id("Q42"), "Q42".to_string());
        assert_eq!(c.get_prefixed_id("P123"), "P123".to_string());
    }

    #[test]
    fn get_statement_id_from_json_id() {
        let c = QuickStatementsCommand::new_from_json(&json!({"id":"Q42$some-guid-here"}));
        let result = c.get_statement_id(&empty_test_item());
        assert_eq!(result, Ok(Some("Q42$some-guid-here".to_string())));
    }

    #[test]
    fn get_statement_id_no_property() {
        let c = QuickStatementsCommand::new_from_json(&json!({}));
        let result = c.get_statement_id(&empty_test_item());
        assert!(result.is_err());
    }

    #[test]
    fn get_statement_id_no_match() {
        let c = QuickStatementsCommand::new_from_json(&json!({
            "property":"P31",
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}}
        }));
        let result = c.get_statement_id(&empty_test_item());
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn action_add_alias_missing_language() {
        let c = QuickStatementsCommand::new_from_json(&json!({"value":"Test"}));
        assert!(c.action_add_alias(&empty_test_item()).is_err());
    }

    #[test]
    fn action_add_alias_missing_value() {
        let c = QuickStatementsCommand::new_from_json(&json!({"language":"en"}));
        assert!(c.action_add_alias(&empty_test_item()).is_err());
    }

    #[test]
    fn action_remove_statement_from_entity() {
        let mut c = QuickStatementsCommand::new_from_json(&json!({
            "action":"remove",
            "what":"statement",
            "property":"P31",
            "datavalue":{"type":"wikibase-entityid","value":{"entity-type":"item","id":"Q42"}}
        }));
        // No matching statement on the empty item
        let result = c.remove_from_entity(&Some(empty_test_item()));
        assert!(result.is_err());
    }

    #[test]
    fn action_remove_bad_what() {
        let mut c =
            QuickStatementsCommand::new_from_json(&json!({"action":"remove","what":"label"}));
        let result = c.remove_from_entity(&Some(empty_test_item()));
        assert!(result.is_err());
    }
}
