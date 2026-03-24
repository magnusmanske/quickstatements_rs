use std::fmt;
use wikibase::{Coordinate, MonoLingualText, QuantityValue, TimeValue};

use crate::entity_id::EntityID;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityID),
    GlobeCoordinate(Coordinate),
    MonoLingualText(MonoLingualText),
    Novalue,
    Quantity(QuantityValue),
    Somevalue,
    String(String),
    Time(TimeValue),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Entity(v) => write!(f, "{}", v),
            Self::GlobeCoordinate(v) => {
                write!(f, "@{}/{}", v.latitude(), v.longitude())
            }
            Self::MonoLingualText(v) => write!(f, "{}:\"{}\"", v.language(), v.text()),
            Self::Novalue => write!(f, "novalue"),
            Self::Quantity(v) => {
                write!(f, "{}", v.amount())?;
                if let (Some(lower), Some(upper)) = (v.lower_bound(), v.upper_bound()) {
                    write!(f, "[{lower},{upper}]")?;
                }
                if v.unit() != "1" {
                    write!(f, "{}", v.unit())?;
                }
                Ok(())
            }
            Self::Somevalue => write!(f, "somevalue"),
            Self::String(v) => write!(f, "\"{}\"", v),
            Self::Time(v) => write!(f, "{}/{}", v.time(), v.precision()),
        }
    }
}

impl Value {
    /// Returns the entity-type string for use in datavalue JSON
    fn entity_type_string(id: &EntityID) -> &'static str {
        match id {
            EntityID::Id(ev) => match ev.entity_type() {
                wikibase::EntityType::Item => "item",
                wikibase::EntityType::Property => "property",
                wikibase::EntityType::Lexeme => {
                    // Check if it's a form or sense sub-entity
                    let id_str = ev.id();
                    if id_str.contains("-F") {
                        "form"
                    } else if id_str.contains("-S") {
                        "sense"
                    } else {
                        "lexeme"
                    }
                }
                wikibase::EntityType::MediaInfo => "mediainfo",
                _ => "item",
            },
            EntityID::Last => "item",
            EntityID::LastForm => "form",
            EntityID::LastSense => "sense",
        }
    }

    /// Returns the datavalue as a JSON value
    pub fn to_json(&self) -> Result<serde_json::Value, String> {
        Ok(match self {
            Self::Entity(id) => json!({
                "type" : "wikibase-entityid",
                "value" : { "entity-type": Self::entity_type_string(id), "id":id.to_string() }
            }),
            Self::Novalue => json!({"value":"novalue","type":"novalue"}),
            Self::Somevalue => json!({"value":"somevalue","type":"somevalue"}),
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
        })
    }
}
