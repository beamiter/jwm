use crate::backend::error::BackendError;

impl From<x11rb::rust_connection::ConnectError> for BackendError {
    fn from(e: x11rb::rust_connection::ConnectError) -> Self {
        BackendError::Other(Box::new(e))
    }
}

impl From<x11rb::rust_connection::ConnectionError> for BackendError {
    fn from(e: x11rb::rust_connection::ConnectionError) -> Self {
        BackendError::Other(Box::new(e))
    }
}

impl From<x11rb::errors::ReplyError> for BackendError {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        BackendError::Other(Box::new(e))
    }
}

impl From<x11rb::errors::ReplyOrIdError> for BackendError {
    fn from(e: x11rb::errors::ReplyOrIdError) -> Self {
        BackendError::Other(Box::new(e))
    }
}
