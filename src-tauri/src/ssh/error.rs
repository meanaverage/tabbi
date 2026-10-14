// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use russh::AgentAuthError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WrappedError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Russh(#[from] russh::Error),

    #[error(transparent)]
    RusshKeys(#[from] russh::keys::Error),

    #[error(transparent)]
    AgentAuthError(#[from] AgentAuthError),

    #[error(transparent)]
    Sftp(#[from] russh_sftp::client::error::Error),

    #[error(transparent)]
    Socks(#[from] tokio_socks::Error),

    #[error(transparent)]
    Http(#[from] async_http_proxy::HttpError),
}

/// The page sees errors as russh-napi reported them, `{err:?}` (tabby-ssh matches some, like `Keys(KeyIsEncrypted)`).
impl From<WrappedError> for String {
    fn from(value: WrappedError) -> Self {
        match value {
            WrappedError::Io(err) => format!("{err:?}"),
            WrappedError::Russh(err) => format!("{err:?}"),
            WrappedError::RusshKeys(err) => format!("{err:?}"),
            WrappedError::AgentAuthError(err) => format!("{err:?}"),
            WrappedError::Sftp(err) => format!("{err:?}"),
            WrappedError::Socks(err) => format!("{err:?}"),
            WrappedError::Http(err) => format!("{err:?}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, String>;

pub fn wrap<E: Into<WrappedError>>(e: E) -> String {
    e.into().into()
}
