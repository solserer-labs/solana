use {
    crate::rpc_request,
    solana_sdk::{
        signature::SignerError, transaction::TransactionError, transport::TransportError,
    },
    std::io,
    thiserror::Error,
};

pub use reqwest; // export `reqwest` for clients

#[derive(Error, Debug)]
pub enum ClientErrorKind {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    RpcError(#[from] rpc_request::RpcError),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::error::Error),
    #[error(transparent)]
    SigningError(#[from] SignerError),
    #[error(transparent)]
    TransactionError(#[from] TransactionError),
    #[error("Custom: {0}")]
    Custom(String),
}

impl From<TransportError> for ClientErrorKind {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::IoError(err) => Self::Io(err),
            TransportError::TransactionError(err) => Self::TransactionError(err),
            TransportError::Custom(err) => Self::Custom(err),
        }
    }
}

impl From<ClientErrorKind> for TransportError {
    fn from(client_error_kind: ClientErrorKind) -> Self {
        match client_error_kind {
            ClientErrorKind::Io(err) => Self::IoError(err),
            ClientErrorKind::TransactionError(err) => Self::TransactionError(err),
            ClientErrorKind::Reqwest(err) => Self::Custom(format!("{:?}", err)),
            ClientErrorKind::RpcError(err) => Self::Custom(format!("{:?}", err)),
            ClientErrorKind::SerdeJson(err) => Self::Custom(format!("{:?}", err)),
            ClientErrorKind::SigningError(err) => Self::Custom(format!("{:?}", err)),
            ClientErrorKind::Custom(err) => Self::Custom(format!("{:?}", err)),
        }
    }
}

#[derive(Error, Debug)]
#[error("{kind}")]
pub struct ClientError {
    pub request: Option<rpc_request::RpcRequest>,

    #[source]
    pub kind: ClientErrorKind,
}

impl ClientError {
    pub fn new_with_request(kind: ClientErrorKind, request: rpc_request::RpcRequest) -> Self {
        Self {
            request: Some(request),
            kind,
        }
    }

    pub fn into_with_request(self, request: rpc_request::RpcRequest) -> Self {
        Self {
            request: Some(request),
            ..self
        }
    }

    pub fn request(&self) -> Option<&rpc_request::RpcRequest> {
        self.request.as_ref()
    }

    pub fn kind(&self) -> &ClientErrorKind {
        &self.kind
    }
}

impl From<ClientErrorKind> for ClientError {
    fn from(kind: ClientErrorKind) -> Self {
        Self {
            request: None,
            kind,
        }
    }
}

impl From<TransportError> for ClientError {
    fn from(err: TransportError) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<ClientError> for TransportError {
    fn from(client_error: ClientError) -> Self {
        client_error.kind.into()
    }
}

impl From<std::io::Error> for ClientError {
    fn from(err: std::io::Error) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(err: reqwest::Error) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<rpc_request::RpcError> for ClientError {
    fn from(err: rpc_request::RpcError) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<serde_json::error::Error> for ClientError {
    fn from(err: serde_json::error::Error) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<SignerError> for ClientError {
    fn from(err: SignerError) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

impl From<TransactionError> for ClientError {
    fn from(err: TransactionError) -> Self {
        Self {
            request: None,
            kind: err.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;
