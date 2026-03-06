use std::fmt;
use wikibase::{Coordinate, MonoLingualText, QuantityValue, TimeValue};

use crate::entity_id::EntityID;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityID),
    GlobeCoordinate(Coordinate),
    MonoLingualText(MonoLingualText),
    Quantity(QuantityValue),
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
            Self::String(v) => write!(f, "\"{}\"", v),
            Self::Time(v) => write!(f, "{}/{}", v.time(), v.precision()),
        }
    }
}

impl Value {
    /// Returns the datavalue as a JSON value
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
        })
    }
}
