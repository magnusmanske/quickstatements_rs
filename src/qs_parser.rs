use regex::Regex;
use wikibase::mediawiki::api::Api;
use wikibase::{
    Coordinate, EntityType, EntityValue, LocaleString, MonoLingualText, SiteLink, TimeValue,
};

use crate::command_type::{CommandModifier, CommandType};
use crate::entity_id::EntityID;
use crate::property_value::PropertyValue;
use crate::value::Value;

pub const COMMONS_API: &str = "https://commons.wikimedia.org/w/api.php";
const GREGORIAN_CALENDAR: &str = "http://www.wikidata.org/entity/Q1985727";
const GLOBE_EARTH: &str = "http://www.wikidata.org/entity/Q2";
const PHP_COMPATIBILITY: bool = true; // TODO

#[derive(Debug, Clone, PartialEq)]
pub struct QuickStatementsParser {
    pub command: CommandType,
    pub item: Option<EntityID>,
    pub target_item: Option<EntityID>, // For MERGE
    pub property: Option<EntityValue>,
    pub value: Option<Value>,
    pub modifier: Option<CommandModifier>,
    pub references: Vec<PropertyValue>,
    pub qualifiers: Vec<PropertyValue>,
    pub sitelink: Option<SiteLink>,
    pub locale_string: Option<LocaleString>,
    pub comment: Option<String>,
    pub create_data: Option<serde_json::Value>,
    // Lexeme-specific fields
    pub lexeme_language: Option<String>,         // Q-id for language
    pub lexeme_category: Option<String>,         // Q-id for lexical category
    pub lemmas: Vec<MonoLingualText>,            // Lemmas (lang:"text" pairs)
    pub representations: Vec<MonoLingualText>,   // Form representations
    pub glosses: Vec<MonoLingualText>,           // Sense glosses
    pub grammatical_features: Vec<String>,       // Q-ids for grammatical features
}

impl QuickStatementsParser {
    /// Translates a line into a QuickStatementsParser object.
    /// Uses api to translate page titles into entity IDs, if given
    pub async fn new_from_line(line: &str, api: Option<&Api>) -> Result<Self, String> {
        lazy_static! {
            static ref RE_META: Regex = Regex::new(r#"^ *([LDAS]) *([a-z_-]+) *$"#).unwrap();
        }

        let (line, comment) = Self::parse_comment(line);
        let mut parts: Vec<String> = line
            .trim()
            .replace("||", "\t")
            .split('\t')
            .map(|s| s.to_string())
            .collect();
        if parts.is_empty() {
            return Err("Empty string".to_string());
        }

        match parts[0].to_uppercase().as_str() {
            "CREATE" => return Self::new_create(comment),
            "CREATE_LEXEME" => return Self::new_create_lexeme(&parts[1..], comment),
            "MERGE" => {
                return Self::new_merge(
                    parts.get(1).map(|s| s.as_str()),
                    parts.get(2).map(|s| s.as_str()),
                    comment,
                )
            }
            _ => {}
        }

        if parts.len() < 3 {
            return Err("No valid command".to_string());
        }

        // Try to convert a page title into an entity ID
        if let Some(id) = Self::get_entity_id_from_title(&parts[0], api).await {
            parts[0] = id
        }

        // Check for lexeme sub-entity commands (second column)
        match parts[1].to_uppercase().as_str() {
            "ADD_FORM" => return Self::new_add_form(&parts, comment),
            "ADD_SENSE" => return Self::new_add_sense(&parts, comment),
            "LEXICAL_CATEGORY" => return Self::new_set_lexical_category(&parts, comment),
            "LANGUAGE" => return Self::new_set_language(&parts, comment),
            _ => {}
        }

        // Check for Lemma_xx, Rep_xx, Gloss_xx patterns
        {
            lazy_static! {
                static ref RE_LEMMA: Regex = Regex::new(r#"(?i)^Lemma_([a-z_-]+)$"#).unwrap();
                static ref RE_REP: Regex = Regex::new(r#"(?i)^Rep_([a-z_-]+)$"#).unwrap();
                static ref RE_GLOSS: Regex = Regex::new(r#"(?i)^Gloss_([a-z_-]+)$"#).unwrap();
                static ref RE_GRAMMATICAL_FEATURE: Regex = Regex::new(r#"(?i)^GRAMMATICAL_FEATURE$"#).unwrap();
            }
            if let Some(caps) = RE_LEMMA.captures(&parts[1]) {
                let lang = caps.get(1).unwrap().as_str().to_lowercase();
                return Self::new_set_lemma(&parts[0], &lang, &parts[2], comment);
            }
            if let Some(caps) = RE_REP.captures(&parts[1]) {
                let lang = caps.get(1).unwrap().as_str().to_lowercase();
                return Self::new_set_form_representation(&parts[0], &lang, &parts[2], comment);
            }
            if let Some(caps) = RE_GLOSS.captures(&parts[1]) {
                let lang = caps.get(1).unwrap().as_str().to_lowercase();
                return Self::new_set_sense_gloss(&parts[0], &lang, &parts[2], comment);
            }
            if RE_GRAMMATICAL_FEATURE.is_match(&parts[1]) {
                return Self::new_set_grammatical_feature(&parts[0], &parts[2], comment);
            }
        }

        if let Some(caps) = RE_META.captures(&parts[1]) {
            let key = caps.get(2).unwrap().as_str();
            let value = match Self::parse_value(parts[2].clone()) {
                Some(Value::String(s)) => s,
                _ => return Err(format!("Bad value: '{}'", &parts[2])),
            };
            let mut ret = Self::new_blank_with_comment(comment.clone());
            let mut first = parts[0].clone();
            ret.modifier = Self::parse_command_modifier(&mut first);
            ret.item = Some(Self::parse_item_id(Some(first.as_str()))?);
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
            create_data: None,
            lexeme_language: None,
            lexeme_category: None,
            lemmas: vec![],
            representations: vec![],
            glosses: vec![],
            grammatical_features: vec![],
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
        Ok(ret)
    }

    fn new_merge(
        i1: Option<&str>,
        i2: Option<&str>,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::Merge;
        ret.item = Some(Self::parse_item_id(i1)?);
        ret.target_item = Some(Self::parse_item_id(i2)?);
        if ret.item.is_none() || ret.target_item.is_none() {
            return Err("MERGE requires two parameters".to_string());
        }
        if ret.item == Some(EntityID::Last) || ret.target_item == Some(EntityID::Last) {
            return Err("MERGE does not allow LAST".to_string());
        }
        Ok(ret)
    }

    fn new_edit_statement(parts: Vec<String>, comment: Option<String>) -> Result<Self, String> {
        lazy_static! {
            static ref RE_PROPERTY: Regex = Regex::new(r#"^[Pp]\d+$"#).unwrap();
        }

        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::EditStatement;
        let mut first = match parts.first() {
            Some(s) => s.trim().to_uppercase(),
            None => return Err(format!("Missing column 1 in {:?}", &parts)),
        };
        ret.modifier = Self::parse_command_modifier(&mut first);
        ret.item = Some(Self::parse_item_id(Some(first.as_str()))?);

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
        &mut self,
        parts: Vec<String>,
        second: String,
    ) -> Result<(), String> {
        self.property = Some(self.parse_property_id(&second)?);
        self.value = Some(match parts.get(2) {
            Some(value) => match Self::parse_value(value.to_string()) {
                Some(value) => value,
                None => return Err("Cannot parse value".to_string()),
            },
            None => return Err("No value given".to_string()),
        });

        // References and qualifiers

        lazy_static! {
            static ref RE_REF_QUAL: Regex = Regex::new(r#"^([PS])(\d+)$"#).unwrap();
        }
        let mut i = parts.iter();
        i.next();
        i.next();
        i.next();
        #[allow(clippy::while_let_loop)]
        loop {
            let (subtype, property) = match i.next() {
                Some(p) => match RE_REF_QUAL.captures(p) {
                    Some(caps) => {
                        let subtype = caps.get(1).unwrap().as_str().to_string();
                        let prop_string = "P".to_string() + caps.get(2).unwrap().as_str();
                        let property = self.parse_property_id(&prop_string)?;
                        (subtype, property)
                    }
                    None => return Err(format!("Bad reference/qualifier key: '{}'", &p)),
                },
                None => break,
            };
            let value = match i.next() {
                Some(v) => match QuickStatementsParser::parse_value(v.to_string()) {
                    Some(value) => value,
                    None => return Err(format!("Cannot parse qualifier/reference value: '{}'", v)),
                },
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

    fn parse_property_id(&self, prop: &str) -> Result<EntityValue, String> {
        let id = Self::parse_item_id(Some(prop))?;
        let ev = match id {
            EntityID::Id(ev) => ev,
            EntityID::Last => return Err("LAST is not a valid property".to_string()),
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

        if !RE_TIME.is_match(value) {
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

        let v = v.replace('T', "-").replace('Z', "").replace(':', "-");
        let mut parts = v.split('-');
        let mut year = parts.next()?.to_string();

        let mut leading_zeros = String::new();
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
            static ref RE_QUANTITY_RANGE: Regex = Regex::new(
                r#"^([-+]{0,1}\d+\.{0,1}\d*)\[([-+]{0,1}\d+\.{0,1}\d*),([-+]{0,1}\d+\.{0,1}\d*)\]$"#
            )
            .unwrap();
        }

        let value = value.to_string();
        let (value, unit) = match RE_QUANTITY_UNIT.captures(&value) {
            Some(caps) => {
                let value = caps.get(1)?.as_str().to_string();
                let unit = format!("http://www.wikidata.org/entity/Q{}", caps.get(2)?.as_str());
                (value, unit)
            }
            None => (value, "1".to_string()),
        };

        if let Some(caps) = RE_QUANTITY_PLAIN.captures(&value) {
            return Some(Value::Quantity(wikibase::QuantityValue::new(
                caps.get(1)?.as_str().parse::<f64>().ok()?,
                None,
                unit,
                None,
            )));
        }

        if let Some(caps) = RE_QUANTITY_TOLERANCE.captures(&value) {
            let amount = caps.get(1)?.as_str().parse::<f64>().ok()?;
            let tolerance = caps.get(2)?.as_str().parse::<f64>().ok()?;
            return Some(Value::Quantity(wikibase::QuantityValue::new(
                amount,
                Some(amount - tolerance),
                unit,
                Some(amount + tolerance),
            )));
        }

        if let Some(caps) = RE_QUANTITY_RANGE.captures(&value) {
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

        if let Some(caps) = RE_COORDINATE.captures(value) {
            return Some(Value::GlobeCoordinate(Coordinate::new(
                None,
                GLOBE_EARTH.to_string(),
                caps.get(1)?.as_str().parse::<f64>().ok()?,
                caps.get(2)?.as_str().parse::<f64>().ok()?,
                None,
            )));
        }

        if let Some(t) = Self::parse_quantity(value) {
            return Some(t);
        }

        if let Some(t) = Self::parse_time(value) {
            return Some(t);
        }

        if let Some(caps) = RE_MONOLINGUAL_STRING.captures(value) {
            // Yes, order 2 then 1 is correct!
            return Some(Value::MonoLingualText(MonoLingualText::new(
                caps.get(2)?.as_str(),
                caps.get(1)?.as_str(),
            )));
        }

        if let Some(caps) = RE_STRING.captures(value) {
            return Some(Value::String(caps.get(1)?.as_str().to_string()));
        }

        if let Ok(id) = Self::parse_item_id(Some(value)) {
            return Some(Value::Entity(id));
        }

        None
    }

    fn parse_command_modifier(first: &mut String) -> Option<CommandModifier> {
        if first.is_empty() {
            return None;
        }
        if first.starts_with('-') {
            let (_, remain) = first.split_at(1);
            *first = remain.trim().to_string();
            return Some(CommandModifier::Remove);
        }
        None
    }

    fn parse_item_id(id: Option<&str>) -> Result<EntityID, String> {
        lazy_static! {
            static ref RE_ENTITY_ID: Regex = Regex::new(r#"^[A-Z]\d+$"#)
                .expect("QuickStatementsParser::parse_item_id:RE_ENTITY_ID does not compile");
            static ref RE_FORM_ID: Regex = Regex::new(r#"^L\d+-F\d+$"#)
                .expect("QuickStatementsParser::parse_item_id:RE_FORM_ID does not compile");
            static ref RE_SENSE_ID: Regex = Regex::new(r#"^L\d+-S\d+$"#)
                .expect("QuickStatementsParser::parse_item_id:RE_SENSE_ID does not compile");
        }
        match id {
            Some(orig_id) => {
                let id = orig_id.trim().to_uppercase();
                if id == "LAST" {
                    return Ok(EntityID::Last);
                }
                // Support L123-F1 (form) and L123-S1 (sense) sub-entity IDs
                if RE_FORM_ID.is_match(&id) || RE_SENSE_ID.is_match(&id) {
                    // Use Lexeme entity type for the base lexeme, store full sub-entity ID
                    let ev = EntityValue::new(EntityType::Lexeme, id);
                    return Ok(EntityID::Id(ev));
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
    async fn get_entity_id_from_title_commons(title: &str, api: &Api) -> Option<String> {
        let params = api.params_into(&[("action", "query"), ("prop", "info"), ("titles", title)]);
        match api.get_query_api_json(&params).await {
            Ok(j) => match j["query"]["pages"].as_object() {
                Some(o) => o
                    .iter()
                    .map(|(page_id, _page_data)| format!("M{}", &page_id))
                    .next(),
                None => None,
            },
            _ => None,
        }
    }

    /// Returns the Wikidata item ID for the given title
    async fn get_entity_id_from_title_wikidata(title: &str, api: &Api) -> Option<String> {
        let params = api.params_into(&[
            ("action", "query"),
            ("prop", "pageprops"),
            ("titles", title),
        ]);
        match api.get_query_api_json(&params).await {
            Ok(j) => match j["query"]["pages"].as_object() {
                Some(o) => o
                    .iter()
                    .filter_map(|(_page_id, page_data)| {
                        page_data["pageprops"]["wikibase_item"].as_str()
                    })
                    .map(|s| s.to_string())
                    .next(),
                None => None,
            },
            _ => None,
        }
    }

    /// Returns a Wikidata or Commons Entity ID for a given title
    async fn get_entity_id_from_title(title: &str, api: Option<&Api>) -> Option<String> {
        match api {
            Some(api) => {
                let mw_title = wikibase::mediawiki::title::Title::new_from_full(title, api);
                if api.api_url() == COMMONS_API && mw_title.namespace_id() == 6 {
                    // File => Mxxx
                    Self::get_entity_id_from_title_commons(title, api).await
                } else {
                    // Generic Wiki page
                    Self::get_entity_id_from_title_wikidata(title, api).await
                }
            }
            None => None,
        }
    }

    /// Parses monolingual text values and Q-ids from a slice of parts.
    /// Returns (monolingual_texts, q_ids).
    fn parse_lexeme_args(parts: &[String]) -> Result<(Vec<MonoLingualText>, Vec<String>), String> {
        lazy_static! {
            static ref RE_MONO: Regex = Regex::new(r#"^([a-z][a-z0-9_-]*):"(.*)"$"#).unwrap();
            static ref RE_QID: Regex = Regex::new(r#"^Q\d+$"#).unwrap();
            static ref RE_QIDS_COMMA: Regex = Regex::new(r#"^Q\d+(,Q\d+)*$"#).unwrap();
        }
        let mut texts = vec![];
        let mut qids = vec![];
        for part in parts {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(caps) = RE_MONO.captures(trimmed) {
                let lang = caps.get(1).unwrap().as_str();
                let text = caps.get(2).unwrap().as_str();
                texts.push(MonoLingualText::new(text, lang));
            } else if RE_QIDS_COMMA.is_match(&trimmed.to_uppercase()) {
                for qid in trimmed.to_uppercase().split(',') {
                    qids.push(qid.to_string());
                }
            } else {
                return Err(format!("Cannot parse lexeme argument: '{}'", trimmed));
            }
        }
        Ok((texts, qids))
    }

    fn new_create_lexeme(parts: &[String], comment: Option<String>) -> Result<Self, String> {
        if parts.len() < 3 {
            return Err("CREATE_LEXEME requires at least language, lexical category, and one lemma".to_string());
        }
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::CreateLexeme;

        // First two args must be Q-ids (language and lexical category)
        let lang_id = parts[0].trim().to_uppercase();
        let cat_id = parts[1].trim().to_uppercase();
        lazy_static! {
            static ref RE_QID: Regex = Regex::new(r#"^Q\d+$"#).unwrap();
        }
        if !RE_QID.is_match(&lang_id) {
            return Err(format!("CREATE_LEXEME: invalid language item '{}'", &parts[0]));
        }
        if !RE_QID.is_match(&cat_id) {
            return Err(format!("CREATE_LEXEME: invalid lexical category item '{}'", &parts[1]));
        }
        ret.lexeme_language = Some(lang_id);
        ret.lexeme_category = Some(cat_id);

        // Remaining args are lemmas (lang:"text")
        let (lemmas, _) = Self::parse_lexeme_args(&parts[2..])?;
        if lemmas.is_empty() {
            return Err("CREATE_LEXEME requires at least one lemma".to_string());
        }
        ret.lemmas = lemmas;
        Ok(ret)
    }

    fn new_add_form(parts: &[String], comment: Option<String>) -> Result<Self, String> {
        // parts[0] = entity (L123 or LAST), parts[1] = ADD_FORM, parts[2..] = representations and grammatical features
        if parts.len() < 3 {
            return Err("ADD_FORM requires at least one representation".to_string());
        }
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::AddForm;
        let first = parts[0].trim().to_uppercase();
        ret.item = Some(Self::parse_item_id(Some(&first))?);

        let (representations, features) = Self::parse_lexeme_args(&parts[2..])?;
        if representations.is_empty() {
            return Err("ADD_FORM requires at least one representation".to_string());
        }
        ret.representations = representations;
        ret.grammatical_features = features;
        Ok(ret)
    }

    fn new_add_sense(parts: &[String], comment: Option<String>) -> Result<Self, String> {
        // parts[0] = entity (L123 or LAST), parts[1] = ADD_SENSE, parts[2..] = glosses
        if parts.len() < 3 {
            return Err("ADD_SENSE requires at least one gloss".to_string());
        }
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::AddSense;
        let first = parts[0].trim().to_uppercase();
        ret.item = Some(Self::parse_item_id(Some(&first))?);

        let (glosses, _) = Self::parse_lexeme_args(&parts[2..])?;
        if glosses.is_empty() {
            return Err("ADD_SENSE requires at least one gloss".to_string());
        }
        ret.glosses = glosses;
        Ok(ret)
    }

    fn new_set_lemma(
        entity: &str,
        lang: &str,
        value: &str,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let value = match Self::parse_value(value.to_string()) {
            Some(Value::String(s)) => s,
            _ => return Err(format!("Bad lemma value: '{}'", value)),
        };
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetLemma;
        ret.item = Some(Self::parse_item_id(Some(&entity.trim().to_uppercase()))?);
        ret.locale_string = Some(LocaleString::new(lang, &value));
        Ok(ret)
    }

    fn new_set_lexical_category(
        parts: &[String],
        comment: Option<String>,
    ) -> Result<Self, String> {
        if parts.len() < 3 {
            return Err("LEXICAL_CATEGORY requires a Q-id value".to_string());
        }
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetLexicalCategory;
        ret.item = Some(Self::parse_item_id(Some(&parts[0].trim().to_uppercase()))?);
        let qid = parts[2].trim().to_uppercase();
        lazy_static! {
            static ref RE_QID: Regex = Regex::new(r#"^Q\d+$"#).unwrap();
        }
        if !RE_QID.is_match(&qid) {
            return Err(format!("LEXICAL_CATEGORY: invalid Q-id '{}'", &parts[2]));
        }
        ret.lexeme_category = Some(qid);
        Ok(ret)
    }

    fn new_set_language(parts: &[String], comment: Option<String>) -> Result<Self, String> {
        if parts.len() < 3 {
            return Err("LANGUAGE requires a Q-id value".to_string());
        }
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetLanguage;
        ret.item = Some(Self::parse_item_id(Some(&parts[0].trim().to_uppercase()))?);
        let qid = parts[2].trim().to_uppercase();
        lazy_static! {
            static ref RE_QID: Regex = Regex::new(r#"^Q\d+$"#).unwrap();
        }
        if !RE_QID.is_match(&qid) {
            return Err(format!("LANGUAGE: invalid Q-id '{}'", &parts[2]));
        }
        ret.lexeme_language = Some(qid);
        Ok(ret)
    }

    fn new_set_form_representation(
        entity: &str,
        lang: &str,
        value: &str,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let value = match Self::parse_value(value.to_string()) {
            Some(Value::String(s)) => s,
            _ => return Err(format!("Bad representation value: '{}'", value)),
        };
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetFormRepresentation;
        ret.item = Some(Self::parse_item_id(Some(&entity.trim().to_uppercase()))?);
        ret.locale_string = Some(LocaleString::new(lang, &value));
        Ok(ret)
    }

    fn new_set_grammatical_feature(
        entity: &str,
        value: &str,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetGrammaticalFeature;
        ret.item = Some(Self::parse_item_id(Some(&entity.trim().to_uppercase()))?);
        lazy_static! {
            static ref RE_QIDS_COMMA: Regex = Regex::new(r#"^Q\d+(,Q\d+)*$"#).unwrap();
        }
        let val = value.trim().to_uppercase();
        if !RE_QIDS_COMMA.is_match(&val) {
            return Err(format!("GRAMMATICAL_FEATURE: invalid Q-id list '{}'", value));
        }
        ret.grammatical_features = val.split(',').map(|s| s.to_string()).collect();
        Ok(ret)
    }

    fn new_set_sense_gloss(
        entity: &str,
        lang: &str,
        value: &str,
        comment: Option<String>,
    ) -> Result<Self, String> {
        let value = match Self::parse_value(value.to_string()) {
            Some(Value::String(s)) => s,
            _ => return Err(format!("Bad gloss value: '{}'", value)),
        };
        let mut ret = Self::new_blank_with_comment(comment);
        ret.command = CommandType::SetSenseGloss;
        ret.item = Some(Self::parse_item_id(Some(&entity.trim().to_uppercase()))?);
        ret.locale_string = Some(LocaleString::new(lang, &value));
        Ok(ret)
    }

    fn parse_comment(line: &str) -> (String, Option<String>) {
        lazy_static! {
            static ref RE_COMMENT: Regex = Regex::new(r#"^(.*)/\*\s*(.*?)\s*\*/(.*)$"#)
                .expect("QuickStatementsParser::parse_comment:RE_COMMENT does not compile");
        }
        match RE_COMMENT.captures(line) {
            Some(caps) => (
                String::from(caps.get(1).unwrap().as_str()) + caps.get(3).unwrap().as_str(),
                Some(caps.get(2).unwrap().as_str().to_string()),
            ),
            None => (line.to_string(), None),
        }
    }

    fn quote(s: &str) -> String {
        format!("\"{}\"", s)
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
                    self.value.clone()?.to_string(),
                ];
                for qualifier in &self.qualifiers {
                    let res = qualifier.to_string_tuple();
                    ret.push(res.0);
                    ret.push(res.1);
                }
                for reference in &self.references {
                    let mut res = reference.to_string_tuple();
                    res.0.replace_range(0..1, "S");
                    ret.push(res.0);
                    ret.push(res.1);
                }
                ret
            }
            CommandType::SetLabel => vec![
                self.item.clone()?.to_string(),
                "L".to_string() + self.locale_string.clone()?.language(),
                Self::quote(self.locale_string.clone()?.value()),
            ],
            CommandType::SetDescription => vec![
                self.item.clone()?.to_string(),
                "D".to_string() + self.locale_string.clone()?.language(),
                Self::quote(self.locale_string.clone()?.value()),
            ],
            CommandType::SetAlias => vec![
                self.item.clone()?.to_string(),
                "A".to_string() + self.locale_string.clone()?.language(),
                Self::quote(self.locale_string.clone()?.value()),
            ],
            CommandType::SetSitelink => vec![
                self.item.clone()?.to_string(),
                "S".to_string() + self.sitelink.clone()?.site(),
                Self::quote(self.sitelink.clone()?.title()),
            ],
            CommandType::CreateLexeme => {
                let mut ret = vec![
                    "CREATE_LEXEME".to_string(),
                    self.lexeme_language.clone()?,
                    self.lexeme_category.clone()?,
                ];
                for lemma in &self.lemmas {
                    ret.push(format!("{}:\"{}\"", lemma.language(), lemma.text()));
                }
                ret
            }
            CommandType::AddForm => {
                let mut ret = vec![
                    self.item.clone()?.to_string(),
                    "ADD_FORM".to_string(),
                ];
                for rep in &self.representations {
                    ret.push(format!("{}:\"{}\"", rep.language(), rep.text()));
                }
                for feat in &self.grammatical_features {
                    ret.push(feat.clone());
                }
                ret
            }
            CommandType::AddSense => {
                let mut ret = vec![
                    self.item.clone()?.to_string(),
                    "ADD_SENSE".to_string(),
                ];
                for gloss in &self.glosses {
                    ret.push(format!("{}:\"{}\"", gloss.language(), gloss.text()));
                }
                ret
            }
            CommandType::SetLemma => vec![
                self.item.clone()?.to_string(),
                format!("Lemma_{}", self.locale_string.clone()?.language()),
                Self::quote(self.locale_string.clone()?.value()),
            ],
            CommandType::SetLexicalCategory => vec![
                self.item.clone()?.to_string(),
                "LEXICAL_CATEGORY".to_string(),
                self.lexeme_category.clone()?,
            ],
            CommandType::SetLanguage => vec![
                self.item.clone()?.to_string(),
                "LANGUAGE".to_string(),
                self.lexeme_language.clone()?,
            ],
            CommandType::SetFormRepresentation => vec![
                self.item.clone()?.to_string(),
                format!("Rep_{}", self.locale_string.clone()?.language()),
                Self::quote(self.locale_string.clone()?.value()),
            ],
            CommandType::SetGrammaticalFeature => vec![
                self.item.clone()?.to_string(),
                "GRAMMATICAL_FEATURE".to_string(),
                self.grammatical_features.join(","),
            ],
            CommandType::SetSenseGloss => vec![
                self.item.clone()?.to_string(),
                format!("Gloss_{}", self.locale_string.clone()?.language()),
                Self::quote(self.locale_string.clone()?.value()),
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

    pub fn to_json(&self) -> Result<Vec<serde_json::Value>, String> {
        match &self.command {
            CommandType::EditStatement => {
                let mut ret = vec![];
                let mut base = json!({"action":self.get_action(),"what":"statement"});
                if let Some(comment) = &self.comment {
                    base["summary"] = json!(comment)
                }
                match &self.item {
                    Some(id) => base["item"] = json!(id.to_string()),
                    None => return Err("No item set".to_string()),
                }
                match &self.property {
                    Some(ev) => base["property"] = json!(ev.id().to_string()), // Assuming property
                    None => return Err("No property set".to_string()),
                }
                match &self.value {
                    Some(value) => base["datavalue"] = value.to_json()?,
                    None => return Err("No value set".to_string()),
                }

                // Short-circuit statement removal
                // TODO reference/qualifier removal?
                if let Some(CommandModifier::Remove) = &self.modifier {
                    ret.push(base.clone());
                    return Ok(ret);
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
                Ok(ret)
            }
            CommandType::Create => {
                let mut ret = json!({"action":"create","type":"item"});
                if let Some(data) = &self.create_data {
                    ret["data"] = data.to_owned();
                }
                Ok(vec![ret])
            }
            CommandType::Merge => match (self.item.as_ref(), self.target_item.as_ref()) {
                (Some(EntityID::Id(item2)), Some(EntityID::Id(item1))) => Ok(vec![
                    json!({"action":"merge","item1":item1.id(),"item2":item2.id(),"type":item1.entity_type()}),
                ]),
                _ => Err(
                    "QuickStatementsParser::to_json:Merge: either item or target_item in None"
                        .to_string(),
                ),
            },
            CommandType::SetLabel => match (self.item.as_ref(), self.locale_string.as_ref()) {
                (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                    json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"label"}),
                ]),
                _ => Err("Label issue".to_string()),
            },
            CommandType::SetDescription => {
                match (self.item.as_ref(), self.locale_string.as_ref()) {
                    (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                        json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"description"}),
                    ]),
                    _ => Err("Description issue".to_string()),
                }
            }
            CommandType::SetAlias => match (self.item.as_ref(), self.locale_string.as_ref()) {
                (Some(EntityID::Id(item)), Some(ls)) => Ok(vec![
                    json!({"action":self.get_action(),"item":item.id(),"language":ls.language(),"value":ls.value(),"what":"alias"}),
                ]),
                _ => Err("Alias issue".to_string()),
            },
            CommandType::SetSitelink => match (self.item.as_ref(), self.sitelink.as_ref()) {
                (Some(EntityID::Id(item)), Some(sl)) => Ok(vec![
                    json!({"action":self.get_action(),"item":item.id(),"site":sl.site(),"value":sl.title(),"what":"sitelink"}),
                ]),
                _ => Err("Sitelink issue".to_string()),
            },
            CommandType::CreateLexeme => {
                let lang = self.lexeme_language.as_ref().ok_or("No language set for CREATE_LEXEME")?;
                let cat = self.lexeme_category.as_ref().ok_or("No lexical category set for CREATE_LEXEME")?;
                let mut lemmas_json = json!({});
                for lemma in &self.lemmas {
                    lemmas_json[lemma.language()] = json!({"language": lemma.language(), "value": lemma.text()});
                }
                let mut data = json!({
                    "lemmas": lemmas_json,
                    "language": lang,
                    "lexicalCategory": cat,
                });
                if let Some(cd) = &self.create_data {
                    // Merge any compressed create data
                    if let Some(obj) = cd.as_object() {
                        for (k, v) in obj {
                            data[k] = v.clone();
                        }
                    }
                }
                let mut ret = json!({"action":"create","type":"lexeme","data":data});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::AddForm => {
                let mut representations = json!({});
                for rep in &self.representations {
                    representations[rep.language()] = json!({"language": rep.language(), "value": rep.text()});
                }
                let mut data = json!({"representations": representations});
                if !self.grammatical_features.is_empty() {
                    data["grammaticalFeatures"] = json!(self.grammatical_features);
                }
                let item_str = self.item.as_ref().ok_or("No item set for ADD_FORM")?.to_string();
                let mut ret = json!({"action":"add","what":"form","item":item_str,"data":data});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::AddSense => {
                let mut glosses_json = json!({});
                for gloss in &self.glosses {
                    glosses_json[gloss.language()] = json!({"language": gloss.language(), "value": gloss.text()});
                }
                let data = json!({"glosses": glosses_json});
                let item_str = self.item.as_ref().ok_or("No item set for ADD_SENSE")?.to_string();
                let mut ret = json!({"action":"add","what":"sense","item":item_str,"data":data});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetLemma => {
                let ls = self.locale_string.as_ref().ok_or("No locale string for SetLemma")?;
                let item_str = self.item.as_ref().ok_or("No item set for SetLemma")?.to_string();
                let mut ret = json!({"action":"add","what":"lemma","item":item_str,"language":ls.language(),"value":ls.value()});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetLexicalCategory => {
                let cat = self.lexeme_category.as_ref().ok_or("No category for SetLexicalCategory")?;
                let item_str = self.item.as_ref().ok_or("No item set for SetLexicalCategory")?.to_string();
                let mut ret = json!({"action":"add","what":"lexical_category","item":item_str,"value":cat});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetLanguage => {
                let lang = self.lexeme_language.as_ref().ok_or("No language for SetLanguage")?;
                let item_str = self.item.as_ref().ok_or("No item set for SetLanguage")?.to_string();
                let mut ret = json!({"action":"add","what":"language","item":item_str,"value":lang});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetFormRepresentation => {
                let ls = self.locale_string.as_ref().ok_or("No locale string for SetFormRepresentation")?;
                let item_str = self.item.as_ref().ok_or("No item set for SetFormRepresentation")?.to_string();
                let mut ret = json!({"action":"add","what":"representation","item":item_str,"language":ls.language(),"value":ls.value()});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetGrammaticalFeature => {
                let item_str = self.item.as_ref().ok_or("No item set for SetGrammaticalFeature")?.to_string();
                let mut ret = json!({"action":"add","what":"grammatical_feature","item":item_str,"value":self.grammatical_features.clone()});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::SetSenseGloss => {
                let ls = self.locale_string.as_ref().ok_or("No locale string for SetSenseGloss")?;
                let item_str = self.item.as_ref().ok_or("No item set for SetSenseGloss")?.to_string();
                let mut ret = json!({"action":"add","what":"gloss","item":item_str,"language":ls.language(),"value":ls.value()});
                if let Some(comment) = &self.comment {
                    ret["summary"] = json!(comment);
                }
                Ok(vec![ret])
            }
            CommandType::Unknown => {
                Err("QuickStatementsParser::to_json:Unknown command is not supported".to_string())
            }
        }
    }

    pub fn compress(commands: &mut Vec<Self>) {
        let mut id_to_merge = 1;

        loop {
            if id_to_merge >= commands.len() {
                break;
            }
            if commands[id_to_merge].item != Some(EntityID::Last)
                || commands[id_to_merge].get_action() != "add"
            {
                id_to_merge += 1;
                continue;
            }
            let create_id = id_to_merge - 1;
            if commands[create_id].command != CommandType::Create
                && commands[create_id].command != CommandType::CreateLexeme
            {
                id_to_merge += 1;
                continue;
            }

            match Self::compress_command_pair(&commands[create_id], &commands[id_to_merge]) {
                Some(create_data) => {
                    commands[create_id].create_data = Some(create_data);
                    commands.remove(id_to_merge);
                }
                None => {
                    id_to_merge += 1;
                }
            }
        }
    }

    fn mainsnak(&self) -> Option<serde_json::Value> {
        let j = self.to_json().ok()?;
        Some(
            json!({"datavalue":j[0]["datavalue"],"snaktype":"value","property":self.property.as_ref().unwrap().id().to_string()}),
        )
    }

    fn compress_add_references_and_qualifiers(
        statement: &mut serde_json::Value,
        merge_command: &Self,
    ) {
        // Qualifiers
        if !merge_command.qualifiers.is_empty() {
            merge_command.qualifiers.iter().for_each(|qual| {
                if !statement["qualifiers"].is_array() {
                    statement["qualifiers"] = json!([]);
                }
                let j = json!({"datavalue":qual.value.to_json().unwrap(),"property":qual.property.id(),"snaktype":"value"});
                statement["qualifiers"].as_array_mut().unwrap().push(j);
            })
        }

        // References
        if !merge_command.references.is_empty() {
            let mut r = json!({"snaks":{}});
            merge_command.references.iter().for_each(|reference|{
                let property = reference.property.id();
                if !r["snaks"][property].is_array() {
                    r["snaks"][property] = json!([]);
                }
                let j = json!({"datavalue":reference.value.to_json().unwrap(),"property":property,"snaktype":"value"});
                r["snaks"][property].as_array_mut().unwrap().push(j);
            });

            if !statement["references"].is_array() {
                statement["references"] = json!([]);
            }
            statement["references"].as_array_mut().unwrap().push(r);
        }
    }

    fn compress_edit_statement(
        cd: serde_json::Value,
        merge_command: &Self,
    ) -> Option<serde_json::Value> {
        let _j = match merge_command.to_json() {
            Ok(j) => j,
            _ => return None,
        };
        let mut cd = cd;
        if !cd["claims"].is_array() {
            cd["claims"] = json!([]);
        }

        let mut statement = match merge_command.mainsnak() {
            Some(mainsnak) => json!({ "mainsnak": mainsnak,"rank":"normal","type":"statement" }),
            None => return None,
        };
        let mut found = false;
        cd["claims"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .for_each(|s| {
                if s["mainsnak"] != statement["mainsnak"] {
                    return;
                }
                found = true;
                Self::compress_add_references_and_qualifiers(s, merge_command);
            });
        if !found {
            Self::compress_add_references_and_qualifiers(&mut statement, merge_command);
            cd["claims"].as_array_mut().unwrap().push(statement);
        }
        Some(cd)
    }

    fn compress_command_pair(
        create_command: &Self,
        merge_command: &Self,
    ) -> Option<serde_json::Value> {
        let mut cd = match &create_command.create_data {
            Some(cd) => cd.clone(),
            None => json!({}),
        };
        match merge_command.command {
            CommandType::EditStatement => Self::compress_edit_statement(cd, merge_command),
            CommandType::SetLabel => match &merge_command.locale_string {
                Some(s) => {
                    cd["labels"][s.language()] = json!(s);
                    Some(cd)
                }
                None => None,
            },
            CommandType::SetDescription => match &merge_command.locale_string {
                Some(s) => {
                    cd["descriptions"][s.language()] = json!(s);
                    Some(cd)
                }
                None => None,
            },
            CommandType::SetAlias => match &merge_command.locale_string {
                Some(s) => {
                    if cd["aliases"][s.language()].is_array() {
                        cd["aliases"][s.language()]
                            .as_array_mut()
                            .unwrap()
                            .push(json!(s));
                    } else {
                        cd["aliases"][s.language()] = json!([s]);
                    }
                    Some(cd)
                }
                None => None,
            },
            CommandType::SetSitelink => match &merge_command.sitelink {
                Some(s) => {
                    cd["sitelinks"][s.site()] = json!({"site":s.site(),"title":s.title()});
                    Some(cd)
                }
                None => None,
            },
            CommandType::SetLemma => match &merge_command.locale_string {
                Some(s) => {
                    cd["lemmas"][s.language()] = json!({"language": s.language(), "value": s.value()});
                    Some(cd)
                }
                None => None,
            },
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_type::{CommandModifier, CommandType};
    use crate::entity_id::EntityID;
    use crate::property_value::PropertyValue;
    use crate::value::Value;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_enwiki_api(server: &MockServer) -> Api {
        let siteinfo: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/siteinfo_enwiki.json")).unwrap();
        Mock::given(method("GET"))
            .and(query_param("action", "query"))
            .and(query_param("meta", "siteinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&siteinfo))
            .mount(server)
            .await;
        Api::new(&format!("{}/w/api.php", server.uri()))
            .await
            .unwrap()
    }

    fn item1() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q123"))
    }

    fn target_item() -> EntityID {
        EntityID::Id(EntityValue::new(EntityType::Item, "Q456"))
    }

    fn make_time(time: &str, precision: u64) -> Option<Value> {
        let time = match PHP_COMPATIBILITY {
            true => time.to_string(),
            false => time.split('T').next().unwrap().to_string() + "00:00:00Z",
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

    #[tokio::test]
    async fn create() {
        let command = "CREATE";
        let mut expected = QuickStatementsParser::new_blank();
        expected.command = CommandType::Create;
        assert_eq!(
            QuickStatementsParser::new_from_line(command, None)
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn merge() {
        let command = "MERGE\tQ123\tQ456";
        let mut expected = QuickStatementsParser::new_blank();
        expected.command = CommandType::Merge;
        expected.item = Some(item1());
        expected.target_item = Some(target_item());
        assert_eq!(
            QuickStatementsParser::new_from_line(command, None)
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    async fn merge_item1_last() {
        let command = "MERGE\tLAST\tQ456";
        QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "MERGE does not allow LAST")]
    async fn merge_item2_last() {
        let command = "MERGE\tQ123\tLAST";
        QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "Not a valid entity ID: BlAH")]
    async fn merge_item1_bad() {
        let command = "MERGE\tBlAH\tQ456";
        QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "Missing value")]
    async fn merge_only_item1() {
        let command = "MERGE\tQ123";
        QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "Not a valid entity ID: ")]
    async fn merge_only_item2() {
        let command = "MERGE\t\tQ456";
        QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
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
            QuickStatementsParser::parse_comment("/* 1234  */\tbar\t"),
            ("\tbar\t".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_end() {
        assert_eq!(
            QuickStatementsParser::parse_comment("\tfoo/* 1234  */"),
            ("\tfoo".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_mid() {
        assert_eq!(
            QuickStatementsParser::parse_comment("\tfoo/* 1234  */\tbar\t"),
            ("\tfoo\tbar\t".to_string(), Some("1234".to_string()))
        );
    }

    #[test]
    fn parse_comment_tight() {
        assert_eq!(
            QuickStatementsParser::parse_comment("\tfoo/*1234*/\tbar\t"),
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

    #[tokio::test]
    async fn title2item() {
        let server = MockServer::start().await;
        let api = mock_enwiki_api(&server).await;

        let title_response: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/title_to_item.json")).unwrap();
        Mock::given(method("GET"))
            .and(query_param("action", "query"))
            .and(query_param("prop", "pageprops"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&title_response))
            .mount(&server)
            .await;

        let command = "Magnus Manske\tP123\tQ456";
        let expected = EntityID::Id(EntityValue::new(EntityType::Item, "Q13520818"));
        assert!(QuickStatementsParser::new_from_line(command, None)
            .await
            .is_err());
        let qsp = QuickStatementsParser::new_from_line(command, Some(&api))
            .await
            .unwrap();
        assert_eq!(qsp.item, Some(expected));
    }

    #[tokio::test]
    async fn title2item_no_match() {
        let server = MockServer::start().await;
        let api = mock_enwiki_api(&server).await;

        let empty_response = serde_json::json!({
            "batchcomplete": "",
            "query": {
                "pages": {
                    "-1": {
                        "ns": 0,
                        "title": "Nonexistent Page 12345",
                        "missing": ""
                    }
                }
            }
        });
        Mock::given(method("GET"))
            .and(query_param("action", "query"))
            .and(query_param("prop", "pageprops"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&empty_response))
            .mount(&server)
            .await;

        let command = "Nonexistent Page 12345\tP123\tQ456";
        // No wikibase_item in pageprops, so title won't resolve and parsing should fail
        assert!(QuickStatementsParser::new_from_line(command, Some(&api))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn file2mediainfo() {
        let command =
            "File:Ruins_of_the_Dower_House,_Fawsley_Park,_Northamptonshire.jpg\tP123\tQ456";
        let api = wikibase::mediawiki::api::Api::new("https://commons.wikimedia.org/w/api.php")
            .await
            .unwrap();
        let expected = EntityID::Id(EntityValue::new(EntityType::MediaInfo, "M82397052"));
        assert!(QuickStatementsParser::new_from_line(command, None)
            .await
            .is_err());
        let qsp = QuickStatementsParser::new_from_line(command, Some(&api))
            .await
            .unwrap();
        assert_eq!(qsp.item, Some(expected));
    }

    #[tokio::test]
    async fn parse_set_label() {
        let command = "Q123\tLen\t\"test label\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLabel);
        assert_eq!(qsp.item, Some(item1()));
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("en", "test label"))
        );
    }

    #[tokio::test]
    async fn parse_set_description() {
        let command = "Q123\tDde\t\"test description\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetDescription);
        assert_eq!(qsp.item, Some(item1()));
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("de", "test description"))
        );
    }

    #[tokio::test]
    async fn parse_set_alias() {
        let command = "Q123\tAit\t\"test alias\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetAlias);
        assert_eq!(qsp.item, Some(item1()));
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("it", "test alias"))
        );
    }

    #[tokio::test]
    async fn parse_set_sitelink() {
        let command = "Q123\tSenwiki\t\"Test Page\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetSitelink);
        assert_eq!(qsp.item, Some(item1()));
        assert_eq!(
            qsp.sitelink,
            Some(SiteLink::new("enwiki", "Test Page", vec![]))
        );
    }

    #[tokio::test]
    async fn parse_edit_statement_simple() {
        let command = "Q123\tP456\tQ789";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(qsp.item, Some(item1()));
        assert_eq!(
            qsp.property,
            Some(EntityValue::new(EntityType::Property, "P456"))
        );
        assert_eq!(
            qsp.value,
            Some(Value::Entity(EntityID::Id(EntityValue::new(
                EntityType::Item,
                "Q789"
            ))))
        );
    }

    #[tokio::test]
    async fn parse_edit_statement_with_qualifier() {
        let command = "Q123\tP456\tQ789\tP321\tQ654";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(qsp.qualifiers.len(), 1);
        assert_eq!(
            qsp.qualifiers[0].property,
            EntityValue::new(EntityType::Property, "P321")
        );
        assert_eq!(
            qsp.qualifiers[0].value,
            Value::Entity(EntityID::Id(EntityValue::new(EntityType::Item, "Q654")))
        );
    }

    #[tokio::test]
    async fn parse_edit_statement_with_reference() {
        let command = "Q123\tP456\tQ789\tS321\tQ654";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(qsp.references.len(), 1);
        assert_eq!(
            qsp.references[0].property,
            EntityValue::new(EntityType::Property, "P321")
        );
        assert_eq!(
            qsp.references[0].value,
            Value::Entity(EntityID::Id(EntityValue::new(EntityType::Item, "Q654")))
        );
    }

    #[tokio::test]
    async fn parse_edit_statement_with_qualifier_and_reference() {
        let command = "Q123\tP456\tQ789\tP321\tQ654\tS143\tQ999";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.qualifiers.len(), 1);
        assert_eq!(qsp.references.len(), 1);
    }

    #[tokio::test]
    async fn parse_remove_statement() {
        let command = "-Q123\tP456\tQ789";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(qsp.modifier, Some(CommandModifier::Remove));
        assert_eq!(qsp.item, Some(item1()));
    }

    #[tokio::test]
    async fn parse_remove_label() {
        let command = "-Q123\tLen\t\"test label\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLabel);
        assert_eq!(qsp.modifier, Some(CommandModifier::Remove));
    }

    #[tokio::test]
    async fn parse_pipe_separator() {
        let command = "Q123||P456||Q789";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(qsp.item, Some(item1()));
    }

    #[tokio::test]
    async fn parse_empty_line() {
        let result = QuickStatementsParser::new_from_line("", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn parse_too_few_parts() {
        let result = QuickStatementsParser::new_from_line("Q123\tP456", None).await;
        assert!(result.is_err());
    }

    #[test]
    fn parse_value_string() {
        assert_eq!(
            QuickStatementsParser::parse_value("\"hello world\"".to_string()),
            Some(Value::String("hello world".to_string()))
        );
    }

    #[test]
    fn parse_value_monolingual_string() {
        assert_eq!(
            QuickStatementsParser::parse_value("en:\"hello world\"".to_string()),
            Some(Value::MonoLingualText(MonoLingualText::new(
                "hello world",
                "en"
            )))
        );
    }

    #[test]
    fn parse_value_entity() {
        assert_eq!(
            QuickStatementsParser::parse_value("Q42".to_string()),
            Some(Value::Entity(EntityID::Id(EntityValue::new(
                EntityType::Item,
                "Q42"
            ))))
        );
    }

    #[test]
    fn parse_value_last() {
        assert_eq!(
            QuickStatementsParser::parse_value("LAST".to_string()),
            Some(Value::Entity(EntityID::Last))
        );
    }

    #[test]
    fn parse_no_comment() {
        assert_eq!(
            QuickStatementsParser::parse_comment("Q123\tP456\tQ789"),
            ("Q123\tP456\tQ789".to_string(), None)
        );
    }

    #[test]
    fn value_display_entity() {
        let v = Value::Entity(EntityID::Id(EntityValue::new(EntityType::Item, "Q42")));
        assert_eq!(v.to_string(), "Q42");
    }

    #[test]
    fn value_display_last() {
        let v = Value::Entity(EntityID::Last);
        assert_eq!(v.to_string(), "LAST");
    }

    #[test]
    fn value_display_string() {
        let v = Value::String("hello".to_string());
        assert_eq!(v.to_string(), "\"hello\"");
    }

    #[test]
    fn value_display_coordinate() {
        let v = Value::GlobeCoordinate(Coordinate::new(
            None,
            GLOBE_EARTH.to_string(),
            1.5,
            -2.5,
            None,
        ));
        assert_eq!(v.to_string(), "@1.5/-2.5");
    }

    #[test]
    fn value_display_monolingual() {
        let v = Value::MonoLingualText(MonoLingualText::new("test", "en"));
        assert_eq!(v.to_string(), "en:\"test\"");
    }

    #[tokio::test]
    async fn generate_qs_line_create() {
        let qsp = QuickStatementsParser::new_from_line("CREATE", None)
            .await
            .unwrap();
        assert_eq!(qsp.generate_qs_line(), Some("CREATE".to_string()));
    }

    #[tokio::test]
    async fn generate_qs_line_merge() {
        let qsp = QuickStatementsParser::new_from_line("MERGE\tQ123\tQ456", None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("MERGE\tQ123\tQ456".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_label() {
        let qsp = QuickStatementsParser::new_from_line("Q123\tLen\t\"test\"", None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("Q123\tLen\t\"test\"".to_string())
        );
    }

    #[tokio::test]
    async fn to_json_create() {
        let qsp = QuickStatementsParser::new_from_line("CREATE", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "create");
        assert_eq!(j[0]["type"], "item");
    }

    #[tokio::test]
    async fn to_json_merge() {
        let qsp = QuickStatementsParser::new_from_line("MERGE\tQ123\tQ456", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "merge");
        assert_eq!(j[0]["item1"], "Q456");
        assert_eq!(j[0]["item2"], "Q123");
    }

    #[tokio::test]
    async fn to_json_label() {
        let qsp = QuickStatementsParser::new_from_line("Q123\tLen\t\"test label\"", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "add");
        assert_eq!(j[0]["what"], "label");
        assert_eq!(j[0]["language"], "en");
        assert_eq!(j[0]["value"], "test label");
    }

    #[tokio::test]
    async fn to_json_description() {
        let qsp = QuickStatementsParser::new_from_line("Q123\tDde\t\"test desc\"", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "description");
    }

    #[tokio::test]
    async fn to_json_alias() {
        let qsp = QuickStatementsParser::new_from_line("Q123\tAit\t\"test alias\"", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "alias");
    }

    #[tokio::test]
    async fn to_json_sitelink() {
        let qsp = QuickStatementsParser::new_from_line("Q123\tSenwiki\t\"Test\"", None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "sitelink");
        assert_eq!(j[0]["site"], "enwiki");
        assert_eq!(j[0]["value"], "Test");
    }

    #[tokio::test]
    async fn to_json_statement_with_qualifier_and_reference() {
        let command = "Q123\tP456\tQ789\tP321\tQ654\tS143\tQ999";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        // statement + qualifier + sources = 3 commands
        assert_eq!(j.len(), 3);
        assert_eq!(j[0]["what"], "statement");
        assert_eq!(j[1]["what"], "qualifier");
        assert_eq!(j[2]["what"], "sources");
    }

    #[tokio::test]
    async fn to_json_remove_statement() {
        let command = "-Q123\tP456\tQ789";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "remove");
    }

    #[tokio::test]
    async fn compress_create_with_label() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("LAST\tLen\t\"test\"", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].create_data.is_some());
        let data = commands[0].create_data.as_ref().unwrap();
        assert!(data["labels"]["en"].is_object());
    }

    #[tokio::test]
    async fn compress_create_with_description() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("LAST\tDde\t\"desc\"", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 1);
        let data = commands[0].create_data.as_ref().unwrap();
        assert!(data["descriptions"]["de"].is_object());
    }

    #[tokio::test]
    async fn compress_create_with_sitelink() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("LAST\tSenwiki\t\"Page\"", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 1);
        let data = commands[0].create_data.as_ref().unwrap();
        assert_eq!(data["sitelinks"]["enwiki"]["title"], "Page");
    }

    #[tokio::test]
    async fn compress_create_with_statement() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("LAST\tP31\tQ5", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 1);
        let data = commands[0].create_data.as_ref().unwrap();
        assert!(data["claims"].is_array());
        assert_eq!(data["claims"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn compress_does_not_merge_non_last() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("Q123\tLen\t\"test\"", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 2);
    }

    #[tokio::test]
    async fn to_json_with_comment() {
        let command = "Q123\tP456\tQ789/* my comment */";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["summary"], "my comment");
    }

    #[test]
    fn property_value_to_string_tuple() {
        let pv = PropertyValue::new(
            EntityValue::new(EntityType::Property, "P123"),
            Value::Entity(EntityID::Id(EntityValue::new(EntityType::Item, "Q456"))),
        );
        assert_eq!(
            pv.to_string_tuple(),
            ("P123".to_string(), "Q456".to_string())
        );
    }

    #[tokio::test]
    async fn parse_bad_qualifier_value() {
        let command = "Q123\tP456\tQ789\tP321\t!!!invalid!!!";
        let result = QuickStatementsParser::new_from_line(command, None).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Cannot parse qualifier/reference value"));
    }

    #[test]
    fn parse_time_no_match() {
        assert_eq!(QuickStatementsParser::parse_time("not-a-time"), None);
    }

    #[test]
    fn parse_quantity_no_match() {
        assert_eq!(
            QuickStatementsParser::parse_quantity("not-a-quantity"),
            None
        );
    }

    #[test]
    fn parse_value_none_for_bad_input() {
        assert_eq!(
            QuickStatementsParser::parse_value("!!!invalid!!!".to_string()),
            None
        );
    }

    #[test]
    fn entity_id_display() {
        let eid = EntityID::Id(EntityValue::new(EntityType::Item, "Q42"));
        assert_eq!(format!("{}", eid), "Q42");
        assert_eq!(format!("{}", EntityID::Last), "LAST");
    }

    #[test]
    fn new_blank_is_unknown() {
        let blank = QuickStatementsParser::new_blank();
        assert_eq!(blank.command, CommandType::Unknown);
        assert_eq!(blank.item, None);
        assert_eq!(blank.generate_qs_line(), None);
    }

    #[test]
    fn to_json_unknown_errors() {
        let blank = QuickStatementsParser::new_blank();
        assert!(blank.to_json().is_err());
    }

    #[test]
    fn get_action_add_and_remove() {
        let mut qsp = QuickStatementsParser::new_blank();
        assert_eq!(qsp.get_action(), "add");
        qsp.modifier = Some(CommandModifier::Remove);
        assert_eq!(qsp.get_action(), "remove");
    }

    #[test]
    fn parse_command_modifier_empty_string() {
        let mut s = String::new();
        assert_eq!(QuickStatementsParser::parse_command_modifier(&mut s), None);
    }

    #[test]
    fn parse_item_id_none() {
        assert!(QuickStatementsParser::parse_item_id(None).is_err());
    }

    #[test]
    fn parse_item_id_last() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("LAST")),
            Ok(EntityID::Last)
        );
    }

    #[test]
    fn parse_item_id_case_insensitive() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("q42")),
            Ok(EntityID::Id(EntityValue::new(EntityType::Item, "Q42")))
        );
    }

    #[test]
    fn parse_item_id_property() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("P123")),
            Ok(EntityID::Id(EntityValue::new(EntityType::Property, "P123")))
        );
    }

    // ========== Lexeme syntax tests ==========

    #[test]
    fn parse_item_id_lexeme() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("L123")),
            Ok(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123")))
        );
    }

    #[test]
    fn parse_item_id_form() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("L123-F1")),
            Ok(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-F1")))
        );
    }

    #[test]
    fn parse_item_id_sense() {
        assert_eq!(
            QuickStatementsParser::parse_item_id(Some("L123-S1")),
            Ok(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-S1")))
        );
    }

    #[tokio::test]
    async fn parse_create_lexeme() {
        let command = "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::CreateLexeme);
        assert_eq!(qsp.lexeme_language, Some("Q7725".to_string()));
        assert_eq!(qsp.lexeme_category, Some("Q1084".to_string()));
        assert_eq!(qsp.lemmas.len(), 1);
        assert_eq!(qsp.lemmas[0].language(), "en");
        assert_eq!(qsp.lemmas[0].text(), "water");
    }

    #[tokio::test]
    async fn parse_create_lexeme_multiple_lemmas() {
        let command = "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"\tfr:\"eau\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::CreateLexeme);
        assert_eq!(qsp.lemmas.len(), 2);
    }

    #[tokio::test]
    async fn parse_create_lexeme_too_few_args() {
        let result = QuickStatementsParser::new_from_line("CREATE_LEXEME\tQ7725", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn parse_create_lexeme_no_lemma() {
        let result =
            QuickStatementsParser::new_from_line("CREATE_LEXEME\tQ7725\tQ1084", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn parse_create_lexeme_bad_language() {
        let result = QuickStatementsParser::new_from_line(
            "CREATE_LEXEME\tBAD\tQ1084\ten:\"water\"",
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn parse_add_form() {
        let command = "L123\tADD_FORM\ten:\"running\"\tQ146786";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::AddForm);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123")))
        );
        assert_eq!(qsp.representations.len(), 1);
        assert_eq!(qsp.representations[0].language(), "en");
        assert_eq!(qsp.representations[0].text(), "running");
        assert_eq!(qsp.grammatical_features, vec!["Q146786"]);
    }

    #[tokio::test]
    async fn parse_add_form_multiple_reps_and_features() {
        let command = "L123\tADD_FORM\ten:\"color\"\ten-gb:\"colour\"\tQ2\tQ3";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.representations.len(), 2);
        assert_eq!(qsp.grammatical_features, vec!["Q2", "Q3"]);
    }

    #[tokio::test]
    async fn parse_add_form_with_last() {
        let command = "LAST\tADD_FORM\ten:\"waters\"\tQ146786";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::AddForm);
        assert_eq!(qsp.item, Some(EntityID::Last));
    }

    #[tokio::test]
    async fn parse_add_sense() {
        let command = "L123\tADD_SENSE\ten:\"transparent liquid\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::AddSense);
        assert_eq!(qsp.glosses.len(), 1);
        assert_eq!(qsp.glosses[0].language(), "en");
        assert_eq!(qsp.glosses[0].text(), "transparent liquid");
    }

    #[tokio::test]
    async fn parse_add_sense_multiple_glosses() {
        let command = "L123\tADD_SENSE\ten:\"transparent liquid\"\tfr:\"liquide transparent\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.glosses.len(), 2);
    }

    #[tokio::test]
    async fn parse_set_lemma() {
        let command = "L123\tLemma_en\t\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLemma);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123")))
        );
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("en", "water"))
        );
    }

    #[tokio::test]
    async fn parse_set_lemma_with_last() {
        let command = "LAST\tLemma_fr\t\"eau\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLemma);
        assert_eq!(qsp.item, Some(EntityID::Last));
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("fr", "eau"))
        );
    }

    #[tokio::test]
    async fn parse_set_lexical_category() {
        let command = "L123\tLEXICAL_CATEGORY\tQ1084";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLexicalCategory);
        assert_eq!(qsp.lexeme_category, Some("Q1084".to_string()));
    }

    #[tokio::test]
    async fn parse_set_language() {
        let command = "L123\tLANGUAGE\tQ7725";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLanguage);
        assert_eq!(qsp.lexeme_language, Some("Q7725".to_string()));
    }

    #[tokio::test]
    async fn parse_set_form_representation() {
        let command = "L123-F1\tRep_en\t\"running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetFormRepresentation);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-F1")))
        );
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("en", "running"))
        );
    }

    #[tokio::test]
    async fn parse_set_grammatical_feature() {
        let command = "L123-F1\tGRAMMATICAL_FEATURE\tQ1,Q2,Q3";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetGrammaticalFeature);
        assert_eq!(qsp.grammatical_features, vec!["Q1", "Q2", "Q3"]);
    }

    #[tokio::test]
    async fn parse_set_sense_gloss() {
        let command = "L123-S1\tGloss_en\t\"act of running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetSenseGloss);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-S1")))
        );
        assert_eq!(
            qsp.locale_string,
            Some(LocaleString::new("en", "act of running"))
        );
    }

    #[tokio::test]
    async fn parse_lexeme_statement() {
        let command = "L123\tP31\tQ5";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123")))
        );
    }

    #[tokio::test]
    async fn parse_form_statement() {
        let command = "L123-F1\tP31\tQ5";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-F1")))
        );
    }

    #[tokio::test]
    async fn parse_sense_statement() {
        let command = "L123-S1\tP31\tQ5";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::EditStatement);
        assert_eq!(
            qsp.item,
            Some(EntityID::Id(EntityValue::new(EntityType::Lexeme, "L123-S1")))
        );
    }

    // ========== Lexeme to_json tests ==========

    #[tokio::test]
    async fn to_json_create_lexeme() {
        let command = "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "create");
        assert_eq!(j[0]["type"], "lexeme");
        assert_eq!(j[0]["data"]["language"], "Q7725");
        assert_eq!(j[0]["data"]["lexicalCategory"], "Q1084");
        assert_eq!(j[0]["data"]["lemmas"]["en"]["value"], "water");
    }

    #[tokio::test]
    async fn to_json_add_form() {
        let command = "L123\tADD_FORM\ten:\"running\"\tQ146786";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "add");
        assert_eq!(j[0]["what"], "form");
        assert_eq!(j[0]["item"], "L123");
        assert_eq!(j[0]["data"]["representations"]["en"]["value"], "running");
        assert_eq!(j[0]["data"]["grammaticalFeatures"][0], "Q146786");
    }

    #[tokio::test]
    async fn to_json_add_sense() {
        let command = "L123\tADD_SENSE\ten:\"transparent liquid\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "add");
        assert_eq!(j[0]["what"], "sense");
        assert_eq!(j[0]["data"]["glosses"]["en"]["value"], "transparent liquid");
    }

    #[tokio::test]
    async fn to_json_set_lemma() {
        let command = "L123\tLemma_en\t\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j[0]["action"], "add");
        assert_eq!(j[0]["what"], "lemma");
        assert_eq!(j[0]["language"], "en");
        assert_eq!(j[0]["value"], "water");
    }

    #[tokio::test]
    async fn to_json_set_lexical_category() {
        let command = "L123\tLEXICAL_CATEGORY\tQ1084";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "lexical_category");
        assert_eq!(j[0]["value"], "Q1084");
    }

    #[tokio::test]
    async fn to_json_set_language() {
        let command = "L123\tLANGUAGE\tQ7725";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "language");
        assert_eq!(j[0]["value"], "Q7725");
    }

    #[tokio::test]
    async fn to_json_set_form_representation() {
        let command = "L123-F1\tRep_en\t\"running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "representation");
        assert_eq!(j[0]["language"], "en");
        assert_eq!(j[0]["value"], "running");
    }

    #[tokio::test]
    async fn to_json_set_grammatical_feature() {
        let command = "L123-F1\tGRAMMATICAL_FEATURE\tQ1,Q2,Q3";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "grammatical_feature");
        assert_eq!(j[0]["value"], json!(["Q1", "Q2", "Q3"]));
    }

    #[tokio::test]
    async fn to_json_set_sense_gloss() {
        let command = "L123-S1\tGloss_en\t\"act of running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        let j = qsp.to_json().unwrap();
        assert_eq!(j[0]["what"], "gloss");
        assert_eq!(j[0]["language"], "en");
        assert_eq!(j[0]["value"], "act of running");
    }

    // ========== Lexeme generate_qs_line tests ==========

    #[tokio::test]
    async fn generate_qs_line_create_lexeme() {
        let command = "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_add_form() {
        let command = "L123\tADD_FORM\ten:\"running\"\tQ146786";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123\tADD_FORM\ten:\"running\"\tQ146786".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_add_sense() {
        let command = "L123\tADD_SENSE\ten:\"transparent liquid\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123\tADD_SENSE\ten:\"transparent liquid\"".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_lemma() {
        let command = "L123\tLemma_en\t\"water\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123\tLemma_en\t\"water\"".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_lexical_category() {
        let command = "L123\tLEXICAL_CATEGORY\tQ1084";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123\tLEXICAL_CATEGORY\tQ1084".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_language() {
        let command = "L123\tLANGUAGE\tQ7725";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123\tLANGUAGE\tQ7725".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_form_representation() {
        let command = "L123-F1\tRep_en\t\"running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123-F1\tRep_en\t\"running\"".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_grammatical_feature() {
        let command = "L123-F1\tGRAMMATICAL_FEATURE\tQ1,Q2,Q3";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123-F1\tGRAMMATICAL_FEATURE\tQ1,Q2,Q3".to_string())
        );
    }

    #[tokio::test]
    async fn generate_qs_line_set_sense_gloss() {
        let command = "L123-S1\tGloss_en\t\"act of running\"";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(
            qsp.generate_qs_line(),
            Some("L123-S1\tGloss_en\t\"act of running\"".to_string())
        );
    }

    // ========== Lexeme command with comment ==========

    #[tokio::test]
    async fn parse_create_lexeme_with_comment() {
        let command = "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\" /* importing English nouns */";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::CreateLexeme);
        assert_eq!(qsp.comment, Some("importing English nouns".to_string()));
    }

    #[tokio::test]
    async fn parse_set_lemma_with_comment() {
        let command = "L123\tLemma_de\t\"Wasser\" /* adding German lemma */";
        let qsp = QuickStatementsParser::new_from_line(command, None)
            .await
            .unwrap();
        assert_eq!(qsp.command, CommandType::SetLemma);
        assert_eq!(qsp.comment, Some("adding German lemma".to_string()));
    }

    // ========== Lexeme compression tests ==========

    #[tokio::test]
    async fn compress_create_lexeme_with_lemma() {
        let mut commands = vec![
            QuickStatementsParser::new_from_line("CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"", None)
                .await
                .unwrap(),
            QuickStatementsParser::new_from_line("LAST\tLemma_fr\t\"eau\"", None)
                .await
                .unwrap(),
        ];
        QuickStatementsParser::compress(&mut commands);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].create_data.is_some());
    }

    // ========== Full example from documentation ==========

    #[tokio::test]
    async fn parse_full_lexeme_example() {
        // All commands from the full example in the spec
        let commands = vec![
            "CREATE_LEXEME\tQ7725\tQ1084\ten:\"water\"",
            "LAST\tLemma_fr\t\"eau\"",
            "LAST\tADD_FORM\ten:\"water\"\tQ110786",
            "LAST\tADD_FORM\ten:\"waters\"\tQ146786",
            "LAST\tADD_SENSE\ten:\"transparent liquid that forms rivers and rain\"",
            "LAST\tP5137\tQ3024658\tS248\tQ328",
        ];
        for cmd in &commands {
            let result = QuickStatementsParser::new_from_line(cmd, None).await;
            assert!(result.is_ok(), "Failed to parse: {}", cmd);
        }
    }
}
