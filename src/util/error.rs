use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)] // TODO: integrate into library error handling
pub enum VexError {
    #[error("index not found at {path}; run `vex index` first")]
    IndexNotFound { path: String },

    #[error("unsupported file type: {ext}")]
    UnsupportedLanguage { ext: String },

    #[error("corrupt index file: {reason}")]
    CorruptIndex { reason: String },

    #[error("embedding model not found at {path}")]
    ModelNotFound { path: String },
}
