use std::fmt;
use wikibase::EntityValue;

#[derive(Debug, Clone, PartialEq)]
pub enum EntityID {
    Id(EntityValue),
    Last,
    LastForm,
    LastSense,
}

impl fmt::Display for EntityID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EntityID::Id(e) => write!(f, "{}", e.id()),
            EntityID::Last => write!(f, "LAST"),
            EntityID::LastForm => write!(f, "LAST_FORM"),
            EntityID::LastSense => write!(f, "LAST_SENSE"),
        }
    }
}
