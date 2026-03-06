use wikibase::EntityValue;

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct PropertyValue {
    pub property: EntityValue,
    pub value: Value,
}

impl PropertyValue {
    pub fn new(property: EntityValue, value: Value) -> Self {
        Self { property, value }
    }

    pub fn to_string_tuple(&self) -> (String, String) {
        (self.property.id().to_string(), self.value.to_string())
    }
}
