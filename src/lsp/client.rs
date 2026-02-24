//! Minimal client-side action model for Pi's LSP-like tooling.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspAction {
    Definition,
    References,
    Symbols,
    Diagnostics,
}

impl LspAction {
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "definition" => Some(Self::Definition),
            "references" => Some(Self::References),
            "symbols" => Some(Self::Symbols),
            "diagnostics" => Some(Self::Diagnostics),
            _ => None,
        }
    }
}
