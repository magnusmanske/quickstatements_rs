use std::fmt;
use wikibase::EntityValue;

#[derive(Debug, Clone, PartialEq)]
pub enum EntityID {
    Id(EntityValue),
    Last,
}

impl fmt::Display for EntityID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EntityID::Id(e) => write!(f, "{}", e.id()),
            EntityID::Last => write!(f, "LAST"),
        }
    }
}
