use regex::Regex;
//use serde_json::Value;
use wikibase::{EntityType, EntityValue};

#[derive(Debug, Clone, PartialEq)]
pub enum EntityID {
    Id(EntityValue),
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityValue),
    GlobeCoordinate,
    MonoLingualText,
    Quantity,
    StringType,
    Time,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandType {
    Create,
    Merge,
    Edit,
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
        println!("COMMENT: {:?}", comment);
        let parts: Vec<String> = line.split("\t").map(|s| s.to_string()).collect();
        if parts.len() == 0 {
            return Err("Empty string".to_string());
        }

        match parts[0].to_uppercase().as_str() {
            "CREATE" => return Self::new_create(),
            "MERGE" => return Self::new_merge(&parts.get(1), &parts.get(2)),
            _ => {}
        }

        if parts.len() >= 3 {
            return Self::new_edit(&parts);
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

    fn new_create() -> Result<Self, String> {
        let mut ret = Self::new_blank();
        ret.command = CommandType::Create;
        return Ok(ret);
    }

    fn new_merge(i1: &Option<&String>, i2: &Option<&String>) -> Result<Self, String> {
        let mut ret = Self::new_blank();
        ret.command = CommandType::Merge;
        ret.item = Some(Self::parse_item_id(i1)?);
        ret.item2 = Some(Self::parse_item_id(i2)?);
        if ret.item.is_none() || ret.item2.is_none() {
            return Err(format!("MERGE requires two parameters"));
        }
        if ret.item == Some(EntityID::Last) || ret.item2 == Some(EntityID::Last) {
            return Err(format!("MERGE does not allow LAST"));
        }
        return Ok(ret);
    }

    fn new_edit(parts: &Vec<String>) -> Result<Self, String> {
        let mut ret = Self::new_blank();
        ret.command = CommandType::Edit;
        let mut first = match parts.get(0) {
            Some(s) => s.trim().to_uppercase(),
            None => return Err(format!("Bad parts: {:?}", &parts)),
        };
        ret.modifier = Self::parse_command_modifier(&mut first);
        Ok(ret)
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
            static ref RE_COMMENT: Regex = Regex::new(r#"/\*\s*(.*?)\s*\*/"#)
                .expect("QuickStatementsParser::parse_comment:RE_COMMENT does not compile");
        }
        match RE_COMMENT.find(&line.to_string()) {
            Some(m) => {
                let start = m.start();
                let end = m.end();
                let comment = m.as_str().to_string();
                let comment = comment[2..comment.len() - 4].trim();
                let mut new_line = line.clone();
                new_line.replace_range(start..end, "");
                return (new_line, Some(comment.to_string()));
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
    fn parse_command_modifier() {
        let mut s = String::from("Q123");
        assert_eq!(QuickStatementsParser::parse_command_modifier(&mut s), None);
        assert_eq!(s, String::from("Q123"));
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
}
