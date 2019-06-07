use regex::Regex;
//use serde_json::Value;
use wikibase::{Coordinate, EntityType, EntityValue, MonoLingualText, QuantityValue, TimeValue};

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

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityID),
    GlobeCoordinate(Coordinate),
    MonoLingualText(MonoLingualText),
    Quantity(QuantityValue),
    String(String),
    Time(TimeValue),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandType {
    Create,
    Merge,
    EditStatement,
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
    item2: Option<EntityID>, // For MERGE
    property: Option<EntityValue>,
    value: Option<Value>,
    modifier: Option<CommandModifier>,
    comment: Option<String>,
}

impl QuickStatementsParser {
    pub fn new_from_line(line: &String) -> Result<Self, String> {
        let (line, comment) = Self::parse_comment(line);
        let parts: Vec<String> = line.split("\t").map(|s| s.to_string()).collect();
        if parts.len() == 0 {
            return Err("Empty string".to_string());
        }

        match parts[0].to_uppercase().as_str() {
            "CREATE" => return Self::new_create(comment),
            "MERGE" => return Self::new_merge(parts.get(1), parts.get(2), comment),
            _ => {}
        }

        if parts.len() >= 3 {
            return Self::new_edit_statement(parts, comment);
        }

        Err("COMMAND NOT VALID".to_string())
    }

    pub fn new_blank() -> Self {
        Self {
            command: CommandType::Unknown,
            item: None,
            item2: None,
            property: None,
            value: None,
            modifier: None,
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
        ret.item2 = Some(Self::parse_item_id(&i2)?);
        if ret.item.is_none() || ret.item2.is_none() {
            return Err(format!("MERGE requires two parameters"));
        }
        if ret.item == Some(EntityID::Last) || ret.item2 == Some(EntityID::Last) {
            return Err(format!("MERGE does not allow LAST"));
        }
        return Ok(ret);
    }

    fn new_edit_statement(parts: Vec<String>, comment: Option<String>) -> Result<Self, String> {
        lazy_static! {
            static ref RE_PROPERTY: Regex = Regex::new(r#"^[Pp]\d+$"#)
                .expect("QuickStatementsParser::new_edit_statement:RE_PROPERTY does not compile");
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

        //ret.property = Some(Self::parse_item_id(property)?);
        Err(format!("Cannot parse commands: {:?}", &parts))
    }

    fn parse_edit_statement_property(
        self: &mut Self,
        parts: Vec<String>,
        second: String,
    ) -> Result<(), String> {
        let id = Self::parse_item_id(&Some(&second))?;
        let ev = match id {
            EntityID::Id(ev) => ev,
            EntityID::Last => return Err(format!("LAST is not a valid property")),
        };
        if *ev.entity_type() != EntityType::Property {
            return Err(format!("{} is not a property", &second));
        }
        self.property = Some(ev);
        self.value = Some(match parts.get(2) {
            Some(value) => match Self::parse_value(value.to_string()) {
                Some(value) => value,
                None => return Err(format!("Cannot parse value")),
            },
            None => return Err(format!("No value given")),
        });

        // TODO ref/qual

        Ok(())
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

        let (v, precision) = match RE_PRECISION.captures(&v) {
            Some(caps) => {
                let new_v = caps.get(1).unwrap().as_str().to_string();
                let p = caps.get(2).unwrap().as_str().parse::<u64>().ok()?;
                (new_v, p)
            }

            None => (v, 9),
        };

        let v = v.replace("T", "-").replace("Z", "").replace(":", "-");
        let mut parts = v.split('-');
        let year = parts.next()?.parse::<u64>().ok()?;
        let month = parts.next().or(Some("1"))?.parse::<u64>().ok()?;
        let day = parts.next().or(Some("1"))?.parse::<u64>().ok()?;
        let hour = parts.next().or(Some("0"))?.parse::<u64>().ok()?;
        let min = parts.next().or(Some("0"))?.parse::<u64>().ok()?;
        let sec = parts.next().or(Some("0"))?.parse::<u64>().ok()?;

        let time = if false {
            // Preserve h/m/s
            format!(
                "{}{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                lead, year, month, day, hour, min, sec
            )
        } else {
            format!(
                "{}{}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                lead, year, month, day, 0, 0, 0
            )
        };

        Some(Value::Time(TimeValue::new(
            0,
            0,
            "http://www.wikidata.org/entity/Q1985727",
            precision,
            &time,
            0,
        )))
    }

    fn parse_value(value: String) -> Option<Value> {
        lazy_static! {
            static ref RE_STRING: Regex = Regex::new(r#"^"(.*)"$"#).unwrap();
            static ref RE_MONOLINGUAL_STRING: Regex = Regex::new(r#"^([a-z-]+):"(.*)"$"#).unwrap();
            static ref RE_COORDINATE: Regex =
                Regex::new(r#"^@([+-]{0,1}[0-9.-]+)/([+-]{0,1}[0-9.-]+)$"#).unwrap();
        }

        /*
        Entity(EntityID),
        MonoLingualText(MonoLingualText),
        String(String),
        Time(TimeValue),
        GlobeCoordinate(Coordinate),
        *Quantity(QuantityValue),
        */

        match RE_COORDINATE.captures(&value) {
            Some(caps) => {
                return Some(Value::GlobeCoordinate(Coordinate::new(
                    None,
                    "http://www.wikidata.org/entity/Q2".to_string(),
                    caps.get(1).unwrap().as_str().parse::<f64>().ok()?,
                    caps.get(2).unwrap().as_str().parse::<f64>().ok()?,
                    None,
                )))
            }
            None => {}
        }

        match Self::parse_time(&value) {
            Some(t) => return Some(t),
            None => {}
        }

        match RE_MONOLINGUAL_STRING.captures(&value) {
            Some(caps) => {
                return Some(Value::MonoLingualText(MonoLingualText::new(
                    caps.get(1).unwrap().as_str(),
                    caps.get(2).unwrap().as_str(),
                )))
            }
            None => {}
        }

        match RE_STRING.captures(&value) {
            Some(caps) => return Some(Value::String(caps.get(1).unwrap().as_str().to_string())),
            None => {}
        }

        match Self::parse_item_id(&Some(&value)) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item1() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q123"))
    }

    fn item2() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q456"))
    }

    fn make_time(time: &str, precision: u64) -> Option<Value> {
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
            QuickStatementsParser::new_from_line(&command.to_string()).unwrap(),
            expected
        );
    }

    #[test]
    fn merge() {
        let command = "MERGE\tQ123\tQ456";
        let mut expected = QuickStatementsParser::new_blank();
        expected.command = CommandType::Merge;
        expected.item = Some(item1());
        expected.item2 = Some(item2());
        assert_eq!(
            QuickStatementsParser::new_from_line(&command.to_string()).unwrap(),
            expected
        );
    }

    #[test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    fn merge_item1_last() {
        let command = "MERGE\tLAST\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string()).unwrap();
    }

    #[test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    fn merge_item2_last() {
        let command = "MERGE\tQ123\tLAST";
        QuickStatementsParser::new_from_line(&command.to_string()).unwrap();
    }

    #[test]
    #[should_panic(expected = "Not a valid entity ID: BlAH")]
    fn merge_item1_bad() {
        let command = "MERGE\tBlAH\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string()).unwrap();
    }

    #[test]
    #[should_panic(expected = "Missing value")]
    fn merge_only_item1() {
        let command = "MERGE\tQ123";
        QuickStatementsParser::new_from_line(&command.to_string()).unwrap();
    }

    #[test]
    #[should_panic(expected = "Not a valid entity ID: ")]
    fn merge_only_item2() {
        let command = "MERGE\t\tQ456";
        QuickStatementsParser::new_from_line(&command.to_string()).unwrap();
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
            make_time("+2019-06-07T00:00:00Z", 8)
        )
    }

    #[test]
    fn parse_time_bce() {
        assert_eq!(
            QuickStatementsParser::parse_time("-2019-06-07T12:13:14Z/8"),
            make_time("-2019-06-07T00:00:00Z", 8)
        )
    }

    #[test]
    fn parse_time_default_precision() {
        assert_eq!(
            QuickStatementsParser::parse_time("+2019-06-07T12:13:14Z"),
            make_time("+2019-06-07T00:00:00Z", 9)
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

}
