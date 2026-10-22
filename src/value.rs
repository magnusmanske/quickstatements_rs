use std::fmt;
use wikimisc::wikibase::{self, Coordinate, MonoLingualText, TimeValue};

use crate::entity_id::EntityID;

/// A quantity with its decimals as written (normalized): an f64 would
/// change amounts with more than 15 significant digits
#[derive(Debug, Clone, PartialEq)]
pub struct Quantity {
    pub amount: String,
    /// Lower and upper bound
    pub bounds: Option<(String, String)>,
    /// "1", or the unit's entity URI
    pub unit: String,
}

impl Quantity {
    /// `amount`, with a tolerance (`~`) or bounds (`[lower,upper]`), all
    /// decimal strings; None if one is not a decimal number
    pub fn new(
        amount: &str,
        tolerance: Option<&str>,
        bounds: Option<(&str, &str)>,
        unit: String,
    ) -> Option<Self> {
        let (a, a_scale) = to_scaled(amount)?;
        let bounds = match (tolerance, bounds) {
            (Some(tolerance), _) => {
                // Exact, unlike PHP's floating point arithmetic
                let (t, t_scale) = to_scaled(tolerance)?;
                let scale = a_scale.max(t_scale);
                let a = rescale(a, scale - a_scale)?;
                let t = rescale(t, scale - t_scale)?;
                Some((
                    from_scaled(a.checked_sub(t)?, scale),
                    from_scaled(a.checked_add(t)?, scale),
                ))
            }
            (None, Some((lower, upper))) => {
                Some((normalize_decimal(lower)?, normalize_decimal(upper)?))
            }
            (None, None) => None,
        };
        Some(Self {
            amount: from_scaled(a, a_scale),
            bounds,
            unit,
        })
    }
}

/// A decimal as Wikibase takes it: no `+`, leading zeros or trailing dot
fn normalize_decimal(s: &str) -> Option<String> {
    let (n, scale) = to_scaled(s)?;
    Some(from_scaled(n, scale))
}

/// A decimal string as an integer and the number of digits after the point
fn to_scaled(s: &str) -> Option<(i128, u32)> {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((format!("{int}{frac}").parse().ok()?, frac.len() as u32))
}

fn rescale(n: i128, extra_digits: u32) -> Option<i128> {
    n.checked_mul(10_i128.checked_pow(extra_digits)?)
}

fn from_scaled(n: i128, scale: u32) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let digits = format!("{:0>width$}", n.unsigned_abs(), width = scale as usize + 1);
    let (int, frac) = digits.split_at(digits.len() - scale as usize);
    match frac {
        "" => format!("{sign}{int}"),
        frac => format!("{sign}{int}.{frac}"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Entity(EntityID),
    GlobeCoordinate(Coordinate),
    MonoLingualText(MonoLingualText),
    Novalue,
    Quantity(Quantity),
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
                write!(f, "{}", v.amount)?;
                if let Some((lower, upper)) = &v.bounds {
                    write!(f, "[{lower},{upper}]")?;
                }
                if let Some((_, id)) = v.unit.rsplit_once("/Q") {
                    write!(f, "U{id}")?;
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
            Self::Quantity(v) => {
                let mut value = json!({"amount": v.amount, "unit": v.unit});
                if let Some((lower, upper)) = &v.bounds {
                    value["lowerBound"] = json!(lower);
                    value["upperBound"] = json!(upper);
                }
                json!({"value": value, "type": "quantity"})
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantity(
        amount: &str,
        tolerance: Option<&str>,
        bounds: Option<(&str, &str)>,
    ) -> Option<Quantity> {
        Quantity::new(amount, tolerance, bounds, "1".to_string())
    }

    #[test]
    fn quantities_keep_their_digits() {
        let q = quantity("12345678901234567890.25", None, None).unwrap();
        assert_eq!(q.amount, "12345678901234567890.25");
        assert_eq!(quantity("+007", None, None).unwrap().amount, "7");
        assert_eq!(quantity("1.50", None, None).unwrap().amount, "1.50");
        assert_eq!(quantity("5.", None, None).unwrap().amount, "5");
        assert_eq!(quantity("-0", None, None).unwrap().amount, "0");
        assert!(quantity("1x", None, None).is_none());
    }

    #[test]
    fn quantity_bounds() {
        let bounds = |q: Option<Quantity>| q.unwrap().bounds.unwrap();
        assert_eq!(
            bounds(quantity("123", Some("2"), None)),
            ("121".into(), "125".into())
        );
        // Exact, unlike floating point (1.2000000000000002)
        assert_eq!(
            bounds(quantity("1.1", Some("0.1"), None)),
            ("1.0".into(), "1.2".into())
        );
        assert_eq!(
            bounds(quantity("-0.321", Some("0.045"), None)),
            ("-0.366".into(), "-0.276".into())
        );
        assert_eq!(
            bounds(quantity("10", None, Some(("+8", "12.0")))),
            ("8".into(), "12.0".into())
        );
    }

    #[test]
    fn quantity_json_and_display() {
        let q = Quantity::new(
            "5",
            Some("1"),
            None,
            "http://www.wikidata.org/entity/Q11573".into(),
        )
        .unwrap();
        let value = Value::Quantity(q);
        assert_eq!(
            value.to_json().unwrap(),
            json!({"type":"quantity","value":{"amount":"5","unit":"http://www.wikidata.org/entity/Q11573","lowerBound":"4","upperBound":"6"}})
        );
        assert_eq!(value.to_string(), "5[4,6]U11573");
    }
}
