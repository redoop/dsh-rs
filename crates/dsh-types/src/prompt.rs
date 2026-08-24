//! System-prompt assembly vocabulary.

/// The result of one assembly: the rendered system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptAssembly {
    pub system: String,
}


/// A provider of a prompt variable's value at assembly time.
pub type VariableProvider = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;
