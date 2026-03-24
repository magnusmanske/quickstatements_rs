#[derive(Debug, Clone, PartialEq)]
pub enum CommandType {
    Create,
    CreateLexeme,
    Merge,
    EditStatement,
    SetLabel,
    SetDescription,
    SetAlias,
    SetSitelink,
    AddForm,
    AddSense,
    SetLemma,
    SetLexicalCategory,
    SetLanguage,
    SetFormRepresentation,
    SetGrammaticalFeature,
    SetSenseGloss,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandModifier {
    Remove,
}
