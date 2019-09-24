use regex::Regex;
//use serde_json::Value;
use wikibase::mediawiki::api::Api;
use wikibase::{
    Coordinate, EntityType, EntityValue, LocaleString, MonoLingualText, QuantityValue, SiteLink,
    TimeValue,
};

pub const COMMONS_API: &str = "https://commons.wikimedia.org/w/api.php";
const GREGORIAN_CALENDAR: &str = "http://www.wikidata.org/entity/Q1985727";
const GLOBE_EARTH: &str = "http://www.wikidata.org/entity/Q2";
const PHP_COMPATIBILITY: bool = true; // TODO

/*
TODO:
Lexemes in the form Lxxx.
Forms in the form Lxxx-Fyy.
Senses in the form Lxxx-Syy.
*/

#[derive(Debug, Clone, PartialEq)]
pub enum EntityID {
    Id(EntityValue),
    Last,
}

impl EntityID {
    pub fn to_string(&self) -> String {
        match self {
            EntityID::Id(e) => e.id().to_string(),
            EntityID::Last => "LAST".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityID),
    GlobeCoordinate(Coordinate),
    MonoLingualText(MonoLingualText),
    Quantity(QuantityValue),
    String(String),
    Time(TimeValue),
}

impl Value {
    pub fn to_string(&self) -> Option<String> {
        lazy_static! {
            static ref RE_UNIT: Regex = Regex::new(r#"/Q(\d+)$"#).unwrap();
        }

        match self {
            Self::Entity(v) => Some(v.to_string()),
            Self::GlobeCoordinate(v) => Some(
                vec![
                    "@".to_string(),
                    v.latitude().to_string(),
                    "/".to_string(),
                    v.longitude().to_string(),
                ]
                .join("")
                .to_string(),
            ),
            Self::MonoLingualText(v) => Some(
                vec![v.language(), ":\"", v.text(), "\""]
                    .join("")
                    .to_string(),
            ),
            Self::Quantity(v) => {
                let mut ret = vec![v.amount().to_string()];
                match (v.lower_bound(), v.upper_bound()) {
                    (Some(lower), Some(upper)) => ret.push(
                        "[".to_string()
                            + &lower.to_string()
                            + &",".to_string()
                            + &upper.to_string()
                            + &"]".to_string(),
                    ),
                    _ => {}
                }
                if v.unit() != "1" {
                    let unit = v.unit().to_string();
                    // TODO captures
                    ret.push(unit);
                }
                Some(ret.join("").to_string())
            }
            Self::String(v) => Some("\"".to_string() + &v + &"\"".to_string()),
            Self::Time(v) => {
                Some(v.time().to_string() + &"/".to_string() + &v.precision().to_string())
            }
        }
    }

    /// INCOMPLETE TODO
    /// Returns the datavalue
    pub fn to_json(&self) -> Result<serde_json::Value, String> {
        Ok(match self {
            Self::Entity(id) => json!({
                "type" : "wikibase-entityid",
                "value" : { "entity-type": "item", "id":id.to_string() }
            }),
            Self::String(v) => json!({"type":"string","value":v.to_string()}),
            Self::Time(v) => json!({"value":v,"type":"time"}),
            Self::GlobeCoordinate(v) => json!({"value":{
                "globe":v.globe(),
                "latitude":v.latitude(),
                "longitude":v.longitude(),
                "precision":1e-6,
            },"type":"globecoordinate"}),
            Self::MonoLingualText(v) => json!({"value":v,"type":"monolingualtext"}),
            Self::Quantity(v) => json!({"value":{
                "amount":format!("{}",v.amount()),
                "unit":v.unit(),
            },"type":"quantity"}),
            //other => return Err(format!("Value::to_json: {:?} is not supported yet", &other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PropertyValue {
    property: EntityValue,
    value: Value,
}

impl PropertyValue {
    pub fn new(property: EntityValue, value: Value) -> Self {
        Self { property, value }
    }

    pub fn to_string_tuple(&self) -> Option<(String, String)> {
        Some((self.property.id().to_string(), self.value.to_string()?))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandType {
    Create,
    Merge,
    EditStatement,
    SetLabel,
    SetDescription,
    SetAlias,
    SetSitelink,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandModifier {
    Remove,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuickStatementsParser {
    command: CommandType,
    item: Option<EntityID>,
    target_item: Option<EntityID>, // For MERGE
    property: Option<EntityValue>,
    value: Option<Value>,
    modifier: Option<CommandModifier>,
    references: Vec<PropertyValue>,
    qualifiers: Vec<PropertyValue>,
    sitelink: Option<SiteLink>,
    locale_string: Option<LocaleString>,
    comment: Option<String>,
}

impl QuickStatementsParser {
    /// Translates a line into a QuickStatementsParser object.
    /// Uses api to translate page titles into entity IDs, if given
    pub fn new_from_line(line: &String, api: Option<&Api>) -> Result<Self, String> {
        lazy_static! {
            static ref RE_META: Regex = Regex::new(r#"^ *([LDAS]) *([a-z_-]+) *$"#).unwrap();
        }

        let (line, comment) = Self::parse_comment(line);
        let mut parts: Vec<String> = line
            .trim()
            .replace("||", "\t")
            .split("\t")
            .map(|s| s.to_string())
            .collect();
        if parts.len() == 0 {
            return Err("Empty string".to_string());
        }

        match parts[0].to_uppercase().as_str() {
            "CREATE" => return Self::new_create(comment),
            "MERGE" => return Self::new_merge(parts.get(1), parts.get(2), comment),
            _ => {}
        }

        if parts.len() < 3 {
            return Err("No valid command".to_string());
        }

        // Try to convert a page title into an entity ID
        match Self::get_entity_id_from_title(&parts[0], api) {
            Some(id) => parts[0] = id,
            None => {}
        }

        match RE_META.captures(&parts[1]) {
            Some(caps) => {
                let key = caps.get(2).unwrap().as_str();
                let value = match Self::parse_value(parts[2].clone()) {
                    Some(Value::String(s)) => s,
                    _ => return Err(format!("Bad value: '{}'", &parts[2])),
                };
                let mut ret = Self::new_blank_with_comment(comment.clone());
                let mut first = parts[0].clone();
                ret.modifier = Self::parse_command_modifier(&mut first);
                ret.item = Some(Self::parse_item_id(&Some(&first))?);
                match caps.get(1).unwrap().as_str() {
                    "L" => {
                        ret.command = CommandType::SetLabel;
                        ret.locale_string = Some(LocaleString::new(key, &value));
                    }
                    "D" => {
                        ret.command = CommandType::SetDescription;
                        ret.locale_string = Some(LocaleString::new(key, &value));
                    }
                    "A" => {
                        ret.command = CommandType::SetAlias;
                        ret.locale_string = Some(LocaleString::new(key, &value));
                    }
                    "S" => {
                        ret.command = CommandType::SetSitelink;
                        ret.sitelink = Some(SiteLink::new(key, &value, vec![]));
                    }
                    _ => return Err(format!("Bad command: '{}'", &parts[1])),
                }
                return Ok(ret);
            }
            None => {}
        }

        Self::new_edit_statement(parts, comment)
    }

    pub fn new_blank() -> Self {
        Self {
            command: CommandType::Unknown,
            item: None,
            target_item: None,
            property: None,
            value: None,
            modifier: None,
            references: vec![],
            qualifiers: vec![],
            sitelink: None,
            locale_string: None,
            comment: None,
        }
    }

    pub fn new_blank_with_comment(comment: Option<String>) -> Self {
        let mut ret = Self::new_blank();
        ret.comment = comment;
        ret
    }

    fn new_create(comment: Option<String>) -> Result<Self, String> {
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::Create;
        return Ok(ret);
    }

    fn new_merge(
        i1: Option<&String>,
        i2: Option<&String>,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::Merge;
        ret.item = Some(Self::parse_item_id(&i1)?);
        ret.target_item = Some(Self::parse_item_id(&i2)?);
        if ret.item.is_none() || ret.target_item.is_none() {
            return Err(format!("MERGE requires two parameters"));
        }
        if ret.item == Some(EntityID::Last) || ret.target_item == Some(EntityID::Last) {
            return Err(format!("MERGE does not allow LAST"));
        }
        return Ok(ret);
    }

    fn new_edit_statement(parts: Vec<String>, comment: Option<String>) -> Result<Self, String> {
        lazy_static! {
            static ref RE_PROPERTY: Regex = Regex::new(r#"^[Pp]\d+$"#).unwrap();
        }

        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::EditStatement;
        let mut first = match parts.get(0) {
            Some(s) => s.trim().to_uppercase(),
            None => return Err(format!("Missing column 1 in {:?}", &parts)),
        };
        ret.modifier = Self::parse_command_modifier(&mut first);
        ret.item = Some(Self::parse_item_id(&Some(&first))?);

        let second = match parts.get(1) {
            Some(s) => s.trim().to_string(),
            None => return Err(format!("Missing column 2 in {:?}", &parts)),
        };

        if RE_PROPERTY.is_match(&second) {
            ret.parse_edit_statement_property(parts, second.to_uppercase())?;
            return Ok(ret);
        }

        Err(format!("Cannot parse commands: {:?}", &parts))
    }

    fn parse_edit_statement_property(
        self: &mut Self,
        parts: Vec<String>,
        second: String,
    ) -> Result<(), String> {
        self.property = Some(self.parse_property_id(&second)?);
        self.value = Some(match parts.get(2) {
            Some(value) => match Self::parse_value(value.to_string()) {
                Some(value) => value,
                None => return Err(format!("Cannot parse value")),
            },
            None => return Err(format!("No value given")),
        });

        // References and qualifiers

        lazy_static! {
            static ref RE_REF_QUAL: Regex = Regex::new(r#"^([PS])(\d+)$"#).unwrap();
        }
        let mut i = parts.iter();
        i.next();
        i.next();
        i.next();
        loop {
            let (subtype, property) = match i.next() {
                Some(p) => match RE_REF_QUAL.captures(p) {
                    Some(caps) => {
                        let subtype = caps.get(1).unwrap().as_str().to_string();
                        let prop_string =
                            "P".to_string() + &caps.get(2).unwrap().as_str().to_string();
                        let property = self.parse_property_id(&prop_string)?;
                        (subtype, property)
                    }
                    None => return Err(format!("Bad reference/qualifier key: '{}'", &p)),
                },
                None => break,
            };
            let value = match i.next() {
                Some(v) => QuickStatementsParser::parse_value(v.to_string()).unwrap(),
                None => {
                    return Err(format!(
                        "Qualifier/Reference key without value: '{:?}'",
                        &property
                    ))
                }
            };
            match subtype.as_str() {
                "S" => self.references.push(PropertyValue::new(property, value)),
                "P" => self.qualifiers.push(PropertyValue::new(property, value)),
                _ => return Err(format!("Bad ref/qual subtype '{}'", &subtype)),
            }
        }

        Ok(())
    }

    fn parse_property_id(&self, prop: &String) -> Result<EntityValue, String> {
        let id = Self::parse_item_id(&Some(&prop))?;
        let ev = match id {
            EntityID::Id(ev) => ev,
            EntityID::Last => return Err(format!("LAST is not a valid property")),
        };
        if *ev.entity_type() != EntityType::Property {
            return Err(format!("{} is not a property", &prop));
        }
        Ok(ev)
    }

    fn parse_time(value: &str) -> Option<Value> {
        lazy_static! {
            static ref RE_TIME: Regex = Regex::new(r#"^[\+\-]{0,1}\d+"#).unwrap();
            static ref RE_PRECISION: Regex = Regex::new(r#"^(.+)/(\d+)$"#).unwrap();
        }

        if !RE_TIME.is_match(&value) {
            return None;
        }

        let mut lead = '+';

        let mut v = value.to_string();
        if v.starts_with('+') {
            v = v[1..].to_string();
        } else if v.starts_with('-') {
            lead = '-';
            v = v[1..].to_string();
        }

        let (v, mut precision) = match RE_PRECISION.captures(&v) {
            Some(caps) => {
                let new_v = caps.get(1)?.as_str().to_string();
                let p = caps.get(2)?.as_str().parse::<u64>().ok()?;
                (new_v, p)
            }

            None => (v, 9),
        };

        let v = v.replace("T", "-").replace("Z", "").replace(":", "-");
        let mut parts = v.split('-');
        let mut year = parts.next()?.to_string();

        let mut leading_zeros = "".to_string();
        while PHP_COMPATIBILITY && year.starts_with('0') && year != "0" {
            leading_zeros += "0";
            year = year[1..].to_string();
        }
        let year = year.parse::<u64>().ok()?;

        let month = parts.next().or(Some("1"))?.parse::<u64>().ok()?;
        let day = parts.next().or(Some("1"))?.parse::<u64>().ok()?;
        let hour = parts.next().or(Some("0"))?.parse::<u64>().ok()?;
        let min = parts.next().or(Some("0"))?.parse::<u64>().ok()?;
        let sec = parts.next().or(Some("0"))?.parse::<u64>().ok()?;

        if precision >= 12 && !PHP_COMPATIBILITY {
            precision = 11;
        }
        if day == 0 && precision >= 11 {
            precision = 10;
        }
        if month == 0 && precision >= 10 {
            precision = 9;
        }

        let time = if PHP_COMPATIBILITY {
            // Preserve h/m/s
            format!(
                "{}{}{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                lead, leading_zeros, year, month, day, hour, min, sec
            )
        } else {
            format!(
                "{}{}{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                lead, leading_zeros, year, month, day, 0, 0, 0
            )
        };

        Some(Value::Time(TimeValue::new(
            0,
            0,
            GREGORIAN_CALENDAR,
            precision,
            &time,
            0,
        )))
    }

    fn parse_quantity(value: &str) -> Option<Value> {
        lazy_static! {
            static ref RE_QUANTITY_UNIT: Regex = Regex::new(r#"^(.+)U(\d+)$"#).unwrap();
            static ref RE_QUANTITY_PLAIN: Regex =
                Regex::new(r#"^([-+]{0,1}\d+\.{0,1}\d*)$"#).unwrap();
            static ref RE_QUANTITY_TOLERANCE: Regex =
                Regex::new(r#"^([-+]{0,1}\d+\.{0,1}\d*)~(\d+\.{0,1}\d*)$"#).unwrap();
            static ref RE_QUANTITY_RANGE: Regex =
                Regex::new(r#"^([-+]{0,1}\d+\.{0,1}\d*)\[([-+]{0,1}\d+\.{0,1}\d*),([-+]{0,1}\d+\.{0,1}\d*)\]$"#).unwrap();
        }

        let value = value.to_string();
        let (value, unit) = match RE_QUANTITY_UNIT.captures(&value) {
            Some(caps) => {
                let value = caps.get(1)?.as_str().to_string();
                let unit = "http://www.wikidata.org/entity/Q".to_string() + &caps.get(2)?.as_str();
                (value, unit)
            }
            None => (value, "1".to_string()),
        };

        match RE_QUANTITY_PLAIN.captures(&value) {
            Some(caps) => {
                return Some(Value::Quantity(wikibase::QuantityValue::new(
                    caps.get(1)?.as_str().parse::<f64>().ok()?,
                    None,
                    unit,
                    None,
                )))
            }
            None => {}
        }

        match RE_QUANTITY_TOLERANCE.captures(&value) {
            Some(caps) => {
                let amount = caps.get(1)?.as_str().parse::<f64>().ok()?;
                let tolerance = caps.get(2)?.as_str().parse::<f64>().ok()?;
                return Some(Value::Quantity(wikibase::QuantityValue::new(
                    amount,
                    Some(amount - tolerance),
                    unit,
                    Some(amount + tolerance),
                )));
            }
            None => {}
        }

        match RE_QUANTITY_RANGE.captures(&value) {
            Some(caps) => {
                let amount = caps.get(1)?.as_str().parse::<f64>().ok()?;
                let lower = caps.get(2)?.as_str().parse::<f64>().ok()?;
                let upper = caps.get(3)?.as_str().parse::<f64>().ok()?;
                return Some(Value::Quantity(wikibase::QuantityValue::new(
                    amount,
                    Some(lower),
                    unit,
                    Some(upper),
                )));
            }
            None => {}
        }

        None
    }

    fn parse_value(value: String) -> Option<Value> {
        lazy_static! {
            static ref RE_STRING: Regex = Regex::new(r#"^"(.*)"$"#).unwrap();
            static ref RE_MONOLINGUAL_STRING: Regex = Regex::new(r#"^([a-z-]+):"(.*)"$"#).unwrap();
            static ref RE_COORDINATE: Regex =
                Regex::new(r#"^@([+-]{0,1}[0-9.-]+)/([+-]{0,1}[0-9.-]+)$"#).unwrap();
        }

        let value = value.trim();

        match RE_COORDINATE.captures(&value) {
            Some(caps) => {
                return Some(Value::GlobeCoordinate(Coordinate::new(
                    None,
                    GLOBE_EARTH.to_string(),
                    caps.get(1)?.as_str().parse::<f64>().ok()?,
                    caps.get(2)?.as_str().parse::<f64>().ok()?,
                    None,
                )))
            }
            None => {}
        }

        match Self::parse_quantity(&value) {
            Some(t) => return Some(t),
            None => {}
        }

        match Self::parse_time(&value) {
            Some(t) => return Some(t),
            None => {}
        }

        match RE_MONOLINGUAL_STRING.captures(&value) {
            Some(caps) => {
                // Yes, order 2 then 1 is correct!
                return Some(Value::MonoLingualText(MonoLingualText::new(
                    caps.get(2)?.as_str(),
                    caps.get(1)?.as_str(),
                )));
            }
            None => {}
        }

        match RE_STRING.captures(&value) {
            Some(caps) => return Some(Value::String(caps.get(1)?.as_str().to_string())),
            None => {}
        }

        match Self::parse_item_id(&Some(&value.to_string())) {
            Ok(id) => return Some(Value::Entity(id)),
            Err(_) => {}
        }

        None
    }

    fn parse_command_modifier(first: &mut String) -> Option<CommandModifier> {
        if first.is_empty() {
            return None;
        }
        if first.starts_with("-") {
            let (_, remain) = first.split_at(1);
            *first = remain.trim().to_string();
            return Some(CommandModifier::Remove);
        }
        None
    }

    fn parse_item_id(id: &Option<&String>) -> Result<EntityID, String> {
        lazy_static! {
            static ref RE_ENTITY_ID: Regex = Regex::new(r#"^[A-Z]\d+$"#)
                .expect("QuickStatementsParser::parse_item_id:RE_ENTITY_ID does not compile");
        }
        match id {
            Some(orig_id) => {
                let id = orig_id.trim().to_uppercase();
                if id == "LAST" {
                    return Ok(EntityID::Last);
                }
                if RE_ENTITY_ID.is_match(&id) {
                    let et = match EntityType::new_from_id(&id) {
                        Ok(et) => et,
                        Err(e) => return Err(format!("{}: {}", &id, &e)),
                    };
                    let ev = EntityValue::new(et, id);
                    Ok(EntityID::Id(ev))
                } else {
                    Err(format!("Not a valid entity ID: {}", &orig_id))
                }
            }
            None => Err("Missing value".to_string()),
        }
    }

    /// Returns the Commons MediaInfo ID for a given file
    fn get_entity_id_from_title_commons(title: &String, api: &Api) -> Option<String> {
        let params = api.params_into(&vec![
            ("action", "query"),
            ("prop", "info"),
            ("titles", title.as_str()),
        ]);
        match api.get_query_api_json(&params) {
            Ok(j) => match j["query"]["pages"].as_object() {
                Some(o) => o
                    .iter()
                    .map(|(page_id, _page_data)| format!("M{}", &page_id))
                    .nth(0),
                None => None,
            },
            _ => None,
        }
    }

    /// Returns the Wikidata item ID for the given title
    fn get_entity_id_from_title_wikidata(title: &String, api: &Api) -> Option<String> {
        let params = api.params_into(&vec![
            ("action", "query"),
            ("prop", "pageprops"),
            ("titles", title.as_str()),
        ]);
        match api.get_query_api_json(&params) {
            Ok(j) => match j["query"]["pages"].as_object() {
                Some(o) => o
                    .iter()
                    .filter_map(|(_page_id, page_data)| {
                        page_data["pageprops"]["wikibase_item"].as_str()
                    })
                    .map(|s| s.to_string())
                    .nth(0),
                None => None,
            },
            _ => None,
        }
    }

    /// Returns a Wikidata or Commons Entity ID for a given title
    fn get_entity_id_from_title(title: &String, api: Option<&Api>) -> Option<String> {
        match api {
            Some(api) => {
                let mw_title = wikibase::mediawiki::title::Title::new_from_full(title, api);
                if api.api_url() == COMMONS_API && mw_title.namespace_id() == 6 {
                    // File => Mxxx
                    Self::get_entity_id_from_title_commons(title, api)
                } else {
                    // Generic Wiki page
                    Self::get_entity_id_from_title_wikidata(title, api)
                }
            }
            None => None,
        }
    }

    fn parse_comment(line: &String) -> (String, Option<String>) {
        lazy_static! {
            static ref RE_COMMENT: Regex = Regex::new(r#"^(.*)/\*\s*(.*?)\s*\*/(.*)$"#)
                .expect("QuickStatementsParser::parse_comment:RE_COMMENT does not compile");
        }
        match RE_COMMENT.captures(&line.to_string()) {
            Some(caps) => {
                return (
                    String::from(caps.get(1).unwrap().as_str()) + caps.get(3).unwrap().as_str(),
                    Some(caps.get(2).unwrap().as_str().to_string()),
                );
            }
            None => (line.to_string(), None),
        }
    }

    fn quote(s: &String) -> String {
        "\"".to_string() + s + "\""
    }

    pub fn generate_qs_line(&self) -> Option<String> {
        let ret = match self.command {
            CommandType::Create => vec!["CREATE".to_string()],
            CommandType::Merge => vec![
                "MERGE".to_string(),
                self.item.clone()?.to_string(),
                self.target_item.clone()?.to_string(),
            ],
            CommandType::EditStatement => {
                let mut ret = vec![
                    self.item.clone()?.to_string(),
                    self.property.clone()?.id().to_string(),
                    self.value.clone()?.to_string()?,
                ];
                for qualifier in &self.qualifiers {
                    let res = qualifier.to_string_tuple()?;
                    ret.push(res.0);
                    ret.push(res.1);
                }
                for reference in &self.references {
                    let mut res = reference.to_string_tuple()?;
                    res.0.replace_range(0..1, "S");
                    ret.push(res.0);
                    ret.push(res.1);
                }
                ret
            }
            CommandType::SetLabel => vec![
                self.item.clone()?.to_string(),
                "L".to_string() + self.locale_string.clone()?.language(),
                Self::quote(&self.locale_string.clone()?.value().to_string()),
            ],
            CommandType::SetDescription => vec![
                self.item.clone()?.to_string(),
                "D".to_string() + self.locale_string.clone()?.language(),
                Self::quote(&self.locale_string.clone()?.value().to_string()),
            ],
            CommandType::SetAlias => vec![
                self.item.clone()?.to_string(),
                "A".to_string() + self.locale_string.clone()?.language(),
                Self::quote(&self.locale_string.clone()?.value().to_string()),
            ],
            CommandType::SetSitelink => vec![
                self.item.clone()?.to_string(),
                "S".to_string() + self.sitelink.clone()?.site(),
                Self::quote(self.sitelink.clone()?.title()),
            ],
            CommandType::Unknown => vec![],
        };
        if ret.is_empty() {
            return None;
        }
        Some(ret.join("\t"))
    }

    pub fn get_action(&self) -> &str {
        match self.modifier {
            Some(CommandModifier::Remove) => "remove",
            _ => "add",
        }
    }

    /// INCOMPLETE TODO
    pub fn to_json(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut ret = vec![];
        match &self.command {
            CommandType::EditStatement => {
                let mut base = json!({"action":self.get_action(),"what":"statement"});
                match &self.comment {
                    Some(comment) => base["summary"] = json!(comment),
                    None => {}
                }
                match &self.item {
                    Some(id) => base["item"] = json!(id.to_string()),
                    None => return Err(format!("No item set")),
                }
                match &self.property {
                    Some(ev) => base["property"] = json!(ev.id().to_string()), // Assuming property
                    None => return Err(format!("No property set")),
                }
                match &self.value {
                    Some(value) => base["datavalue"] = value.to_json()?,
                    None => return Err(format!("No value set")),
                }

                // Short-circuit statement removal
                // TODO reference/qualifier removal?
                match &self.modifier {
                    Some(CommandModifier::Remove) => {
                        ret.push(base.clone());
                        return Ok(ret);
                    }
                    _ => {}
                }

                // Adding only from here on
                ret.push(base.clone());

                // Qualifiers
                if !self.qualifiers.is_empty() {
                    self.qualifiers.iter().for_each(|qual| {
                        let mut command = base.clone();
                        command["what"] = json!("qualifier");
                        command["qualifier"] = json!({
                            "prop":qual.property.id(), // Assuming property
                            "value":qual.value.to_json().unwrap(),
                        });
                        ret.push(command.clone());
                    })
                }

                // References
                if !self.references.is_empty() {
                    let mut command = base.clone();
                    command["what"] = json!("sources");
                    let sources: Vec<serde_json::Value> = self
                        .references
                        .iter()
                        .map(|reference| {
                            json!({
                                "prop":reference.property.id(), // Assuming property
                                "value":reference.value.to_json().unwrap(),
                            })
                        })
                        .collect();
                    command["sources"] = json!(sources);
                    ret.push(command.clone());
                }
            }
            CommandType::Merge => {
                return match (self.item.as_ref(), self.target_item.as_ref()) {
                    (Some(EntityID::Id(item2)), Some(EntityID::Id(item1))) => Ok(vec![
                        json!({"action":"merge","item1":item1.id(),"item2":item2.id(),"type":item1.entity_type()}),
                    ]),
                    _ => Err(format!(
                        "QuickStatementsParser::to_json:Merge: either item or target_item in None"
                    )),
                }
            }
            CommandType::SetLabel => {
                return match (self.item.as_ref(), self.locale_string.as_ref()) {
                    (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                        json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"label"}),
                    ]),
                    _ => Err(format!("Label issue")),
                }
            }
            CommandType::SetDescription => {
                return match (self.item.as_ref(), self.locale_string.as_ref()) {
                    (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                        json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"description"}),
                    ]),
                    _ => Err(format!("Description issue")),
                }
            }
            CommandType::SetAlias => {
                return match (self.item.as_ref(), self.locale_string.as_ref()) {
                    (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                        json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"alias"}),
                    ]),
                    _ => Err(format!("Alias issue")),
                }
            }
            CommandType::SetSitelink => {
                return match (self.item.as_ref(), self.sitelink.as_ref()) {
                    (Some(EntityID::Id(item)), Some(sl)) => Ok(vec![
                        json!({"action":self.get_action(),"item":item.id(),"site":sl.site(),"value":sl.title(),"what":"sitelink"}),
                    ]),
                    _ => Err(format!("Sitelink issue")),
                }
            }
            other => {
                return Err(format!(
                    "QuickStatementsParser::to_json:{:?} is not supported yet",
                    &other
                ))
            }
        }
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item1() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q123"))
    }

    fn target_item() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q456"))
    }

    fn make_time(time: &str, precision: u64) -> Option<Value> {
        let time = match PHP_COMPATIBILITY {
            true => time.to_string(),
            false => time.split('T').nth(0).unwrap().to_string() + "00:00:00Z",
        };
        Some(Value::Time(TimeValue::new(
            0,
            0,
            "http://www.wikidata.org/entity/Q1985727",
            precision,
            &time.to_string(),
            0,
        )))
    }

    fn make_coordinate(lat: f64, lon: f64) -> Option<Value> {
        Some(Value::GlobeCoordinate(Coordinate::new(
            None,
            "http://www.wikidata.org/entity/Q2".to_string(),
            lat,
            lon,
            None,
        )))
    }

    #[test]
    fn create() {
        let command = "CREATE";
        let mut expected = QuickStatementsParser::new_blank();
        expected.command = CommandType::Create;
        assert_eq!(
            QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap(),
            expected
        );
    }

    #[test]
    fn merge() {
        let command = "MERGE\tQ123\tQ456";
        let mut expected = QuickStatementsParser::new_blank();
        expected.command = CommandType::Merge;
        expected.item = Some(item1());
        expected.target_item = Some(target_item());
        assert_eq!(
            QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap(),
            expected
        );
    }

    #[test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    fn merge_item1_last() {
        let command = "MERGE\tLAST\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap();
    }

    #[test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    fn merge_item2_last() {
        let command = "MERGE\tQ123\tLAST";
        QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap();
    }

    #[test]
    #[should_panic(expected = "Not a valid entity ID: BlAH")]
    fn merge_item1_bad() {
        let command = "MERGE\tBlAH\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap();
    }

    #[test]
    #[should_panic(expected = "Missing value")]
    fn merge_only_item1() {
        let command = "MERGE\tQ123";
        QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap();
    }

    #[test]
    #[should_panic(expected = "Not a valid entity ID: ")]
    fn merge_only_item2() {
        let command = "MERGE\t\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string(), None).unwrap();
    }

    #[test]
    fn parse_command_modifier_none() {
        let mut s = String::from("Q123");
        assert_eq!(QuickStatementsParser::parse_command_modifier(&mut s), None);
        assert_eq!(s, String::from("Q123"));
    }

    #[test]
    fn parse_command_modifier_remove() {
        let mut s = String::from("- Q123");
        assert_eq!(
            QuickStatementsParser::parse_command_modifier(&mut s),
            Some(CommandModifier::Remove)
        );
        assert_eq!(s, String::from("Q123"));
    }

    #[test]
    fn parse_comment_start() {
        assert_eq!(
            QuickStatementsParser::parse_comment(&"/* 1234  */\tbar\t".to_string()),
            ("\tbar\t".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_end() {
        assert_eq!(
            QuickStatementsParser::parse_comment(&"\tfoo/* 1234  */".to_string()),
            ("\tfoo".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_mid() {
        assert_eq!(
            QuickStatementsParser::parse_comment(&"\tfoo/* 1234  */\tbar\t".to_string()),
            ("\tfoo\tbar\t".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_tight() {
        assert_eq!(
            QuickStatementsParser::parse_comment(&"\tfoo/*1234*/\tbar\t".to_string()),
            ("\tfoo\tbar\t".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_time_full() {
        assert_eq!(
            QuickStatementsParser::parse_time("+2019-06-07T12:13:14Z/8"),
            make_time("+2019-06-07T12:13:14Z", 8)
        )
    }

    #[test]
    fn parse_time_bce() {
        assert_eq!(
            QuickStatementsParser::parse_time("-2019-06-07T12:13:14Z/8"),
            make_time("-2019-06-07T12:13:14Z", 8)
        )
    }

    #[test]
    fn parse_time_default_precision() {
        assert_eq!(
            QuickStatementsParser::parse_time("+2019-06-07T12:13:14Z"),
            make_time("+2019-06-07T12:13:14Z", 9)
        )
    }

    #[test]
    fn parse_time_day() {
        assert_eq!(
            QuickStatementsParser::parse_time("+2019-06-07/11"),
            make_time("+2019-06-07T00:00:00Z", 11)
        )
    }

    #[test]
    fn parse_time_year() {
        assert_eq!(
            QuickStatementsParser::parse_time("+2019"),
            make_time("+2019-01-01T00:00:00Z", 9)
        )
    }

    #[test]
    fn parse_coordinate() {
        assert_eq!(
            QuickStatementsParser::parse_value("@-123.45/67.89".to_string()),
            make_coordinate(-123.45, 67.89)
        )
    }

    #[test]
    fn parse_quantity_plain() {
        assert_eq!(
            QuickStatementsParser::parse_value("-0.123".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                -0.123, None, "1", None
            )))
        )
    }

    #[test]
    fn parse_quantity_unit() {
        assert_eq!(
            QuickStatementsParser::parse_value("-0.123U11573".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                -0.123,
                None,
                "http://www.wikidata.org/entity/Q11573",
                None
            )))
        )
    }

    #[test]
    fn parse_quantity_tolerance() {
        assert_eq!(
            QuickStatementsParser::parse_value("-0.321~0.045".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                -0.321,
                Some(-0.366),
                "1",
                Some(-0.276)
            )))
        )
    }

    #[test]
    fn parse_quantity_tolerance_unit() {
        assert_eq!(
            QuickStatementsParser::parse_value("-0.321~0.045U123".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                -0.321,
                Some(-0.366),
                "http://www.wikidata.org/entity/Q123",
                Some(-0.276)
            )))
        )
    }

    #[test]
    fn parse_quantity_range() {
        assert_eq!(
            QuickStatementsParser::parse_value("4.56[-1.23,7.89]".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                4.56,
                Some(-1.23),
                "1",
                Some(7.89)
            )))
        )
    }

    #[test]
    fn parse_quantity_range_unit() {
        assert_eq!(
            QuickStatementsParser::parse_value("4.56[-1.23,7.89]U456".to_string()),
            Some(Value::Quantity(wikibase::QuantityValue::new(
                4.56,
                Some(-1.23),
                "http://www.wikidata.org/entity/Q456",
                Some(7.89)
            )))
        )
    }

    #[test]
    fn title2item() {
        let command = "Magnus Manske\tP123\tQ456";
        let api = wikibase::mediawiki::api::Api::new("https://en.wikipedia.org/w/api.php").unwrap();
        let expected = EntityID::Id(EntityValue::new(EntityType::Item, "Q13520818"));
        assert!(QuickStatementsParser::new_from_line(&command.to_string(), None).is_err());
        let qsp = QuickStatementsParser::new_from_line(&command.to_string(), Some(&api)).unwrap();
        assert_eq!(qsp.item, Some(expected));
    }

    #[test]
    fn file2mediainfo() {
        let command =
            "File:Ruins_of_the_Dower_House,_Fawsley_Park,_Northamptonshire.jpg\tP123\tQ456";
        let api =
            wikibase::mediawiki::api::Api::new("https://commons.wikimedia.org/w/api.php").unwrap();
        let expected = EntityID::Id(EntityValue::new(EntityType::MediaInfo, "M82397052"));
        assert!(QuickStatementsParser::new_from_line(&command.to_string(), None).is_err());
        let qsp = QuickStatementsParser::new_from_line(&command.to_string(), Some(&api)).unwrap();
        assert_eq!(qsp.item, Some(expected));
    }

    // TODO add label/alias/desc/sitelink
    // TODO sources
    // TODO qualifiers

}
