//! Operation failures shared by domains. The process host chooses exit status and rendering.

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Failed,
    Invalid,
    Unsupported,
    CannotRun,
}

#[derive(Debug)]
pub struct OperationError {
    kind: Kind,
    message: String,
}
impl OperationError {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}
impl std::fmt::Display for OperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for OperationError {}

pub fn error(kind: Kind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(OperationError { kind, message: message.into() })
}
