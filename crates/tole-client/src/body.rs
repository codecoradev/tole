//! The request body as the client sends it: any serializable value as JSON bytes.

use crate::error::Result;
use serde::Serialize;

/// Object-safe view of a serializable request body.
pub(crate) trait ErasedBody {
    fn to_json(&self) -> Result<Vec<u8>>;
}

impl<T: Serialize> ErasedBody for T {
    fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}
