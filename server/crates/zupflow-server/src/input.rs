//! Request bodies: JSON, read as a value and checked field by field, so every
//! invalid field gets its own message in `fields`, as the contract's
//! `validation_failed` describes. Unknown fields are ignored.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use serde_json::{Map, Value};
use zupflow_core::error::ApiError;

use crate::http::Failure;

/// The largest body any route here reads.
const MAX_BODY_BYTES: usize = 64 << 10;

/// A JSON object body.
pub struct Body(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for Body {
    type Rejection = Failure;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let invalid = || {
            Failure(ApiError::new(
                400,
                "invalid_json",
                "The request body is not valid JSON.",
            ))
        };
        // JSON only, so a cross-site form or `text/plain` post can't carry a
        // body without the browser asking first (a CORS preflight).
        let json = request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return Err(ApiError::new(
                415,
                "unsupported_media_type",
                "Send the body as JSON, with Content-Type: application/json.",
            )
            .into());
        }
        let bytes = Bytes::from_request(request, state).await.map_err(|_| invalid())?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(ApiError::new(413, "body_too_large", "The request body is too large.").into());
        }
        match serde_json::from_slice(&bytes) {
            Ok(Value::Object(object)) => Ok(Self(object)),
            Ok(_) => Err(
                ApiError::validation(BTreeMap::from([("body".to_owned(), "Send a JSON object.".to_owned())])).into(),
            ),
            Err(_) => Err(invalid()),
        }
    }
}

/// Collects per-field problems while a body is checked.
#[derive(Default)]
pub struct Fields(BTreeMap<String, String>);

impl Fields {
    pub fn invalid(&mut self, field: &str, message: &str) {
        self.0.entry(field.to_owned()).or_insert_with(|| message.to_owned());
    }

    /// `Err(validation_failed)` if any field was invalid.
    pub fn finish(self) -> Result<(), Failure> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ApiError::validation(self.0).into())
        }
    }
}

impl Body {
    /// A string field, `None` if absent. Present but not a string is invalid.
    pub fn optional_string(&self, field: &str, fields: &mut Fields) -> Option<String> {
        match self.0.get(field) {
            None => None,
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => {
                fields.invalid(field, "Must be a string.");
                None
            }
        }
    }

    /// A required string field.
    pub fn string(&self, field: &str, fields: &mut Fields) -> Option<String> {
        let value = self.optional_string(field, fields);
        if value.is_none() && !self.0.contains_key(field) {
            fields.invalid(field, "Required.");
        }
        value
    }
}
