//! Request bodies: JSON, read as a value and checked field by field, so every
//! invalid field gets its own message in `fields`, as the contract's
//! `validation_failed` describes. Unknown fields are ignored.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use serde_json::{Map, Value};
use zupflow_core::error::ApiError;

use crate::http::Failure;

/// The largest body a route reads, unless it says otherwise.
pub const MAX_BODY_BYTES: usize = 64 << 10;

/// The largest delivery a channel provider's event may be: Slack's events
/// can carry far more than an API request (a long message with its blocks
/// and attachments).
pub const MAX_PROVIDER_EVENT_BYTES: usize = 1 << 20;

/// A request body, read but not yet interpreted: routes check the caller and
/// the role first, then ask for the JSON object, so a refused caller gets
/// `403` before any complaint about what they sent. Read up to `LIMIT`
/// bytes, and refused past it.
pub struct Body<const LIMIT: usize = MAX_BODY_BYTES> {
    json: bool,
    bytes: Bytes,
}

/// A channel provider's delivery, read up to [`MAX_PROVIDER_EVENT_BYTES`]:
/// for the inbound endpoint only.
pub type ProviderEventBody = Body<MAX_PROVIDER_EVENT_BYTES>;

impl<S: Send + Sync, const LIMIT: usize> FromRequest<S> for Body<LIMIT> {
    type Rejection = Failure;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let json = request
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
        let bytes = axum::body::to_bytes(request.into_body(), LIMIT)
            .await
            .map_err(|_| ApiError::new(413, "body_too_large", "The request body is too large."))?;
        Ok(Self { json, bytes })
    }
}

impl<const LIMIT: usize> Body<LIMIT> {
    /// The body exactly as sent, for a route whose caller signed these
    /// bytes (a channel provider's delivery): read only to check that
    /// signature, and interpreted only after it's accepted.
    pub fn raw(&self) -> &[u8] {
        &self.bytes
    }

    /// The body as a JSON object. JSON only, so a cross-site form or
    /// `text/plain` post can't carry a body without the browser asking first
    /// (a CORS preflight).
    pub fn object(&self) -> Result<Object, Failure> {
        if !self.json {
            return Err(ApiError::new(
                415,
                "unsupported_media_type",
                "Send the body as JSON, with Content-Type: application/json.",
            )
            .into());
        }
        match serde_json::from_slice(&self.bytes) {
            Ok(Value::Object(object)) => Ok(Object(object)),
            Ok(_) => Err(
                ApiError::validation(BTreeMap::from([("body".to_owned(), "Send a JSON object.".to_owned())])).into(),
            ),
            Err(_) => Err(ApiError::new(400, "invalid_json", "The request body is not valid JSON.").into()),
        }
    }
}

/// A JSON object body.
pub struct Object(pub Map<String, Value>);

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

impl Object {
    /// Whether the body names `field` at all.
    pub fn has(&self, field: &str) -> bool {
        self.0.contains_key(field)
    }

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
