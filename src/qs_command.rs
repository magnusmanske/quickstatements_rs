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

    pub fn action_set_sitelink(self: &mut Self, item: &wikibase::Entity) -> Result<Value, String> {
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
                        return Ok(json!({"already_done":1}));
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

    pub fn action_add_statement(&self, q: &str) -> Result<Value, String> {
        let property = match self.json["property"].as_str() {
            Some(p) => p.to_owned(),
            None => return Err("Property not found".to_string()),
        };
        let value = serde_json::to_string(&self.json["datavalue"]["value"])
            .map_err(|e| format!("{:?}", e))?;

        Ok(json!({
            "action":"wbcreateclaim",
            "entity":self.get_prefixed_id(q),
            "snaktype":self.get_snak_type_for_datavalue(&self.json["datavalue"])?,
            "property":property,
            "value":value
        }))
    }

    pub fn action_create_entity(&self) -> Result<Value, String> {
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

    pub fn action_merge_entities(&self) -> Result<Value, String> {
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

    pub fn is_same_datavalue(&self, dv1: &wikibase::DataValue, dv2: &Value) -> Option<bool> {
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
            wikibase::Value::MonoLingual(v) => Some(
                v.language() == v2["language"].as_str()?
                    && self.normalize_string(&v.text().to_string())
                        == self.normalize_string(&v2["text"].as_str()?.to_string()),
            ),
            wikibase::Value::Entity(v) => Some(v.id() == v2["id"].as_str()?),
            wikibase::Value::Quantity(v) => {
                Some(*v.amount() == v2["amount"].as_str()?.parse::<f64>().ok()?)
            }
            wikibase::Value::StringValue(v) => Some(
                self.normalize_string(&v.to_string())
                    == self.normalize_string(&v2.as_str()?.to_string()),
            ),
            wikibase::Value::Time(v) => {
                let t1 = RE_TIME.replace_all(v.time(), "$a$b");
                let t2 = RE_TIME.replace_all(v2["time"].as_str()?, "$a$b");
                Some(v.calendarmodel() == v2["calendarmodel"].as_str()? && t1 == t2)
            }
        }
    }

    fn normalize_string(&self, s: &String) -> String {
        // TODO necessary?
        // In PHP: normalizer_normalize (using Form D)
        s.to_string()
    }

    pub fn get_prefixed_id(&self, s: &str) -> String {
        s.to_string() // TODO necessary?
    }

    pub fn get_snak_type_for_datavalue(&self, dv: &Value) -> Result<String, String> {
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

    pub fn get_statement_id(&self, item: &wikibase::Entity) -> Result<Option<String>, String> {
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

    pub fn check_prop(&self, s: &str) -> Result<String, String> {
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
