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
