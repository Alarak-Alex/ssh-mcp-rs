pub mod core;
pub mod files;
pub mod forwards;
pub mod sessions;
pub mod transfers;

#[derive(Debug)]
pub struct Error {
    pub kind: &'static str,
    pub message: String,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
impl Error {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("validation_error", message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
    pub fn remote_file(message: impl Into<String>) -> Self {
        Self::new("remote_file_error", message)
    }
}
impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::internal(value.to_string())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}
