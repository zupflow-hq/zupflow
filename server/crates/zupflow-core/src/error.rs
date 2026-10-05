//! The API's errors: a status, a stable `snake_case` code clients branch on,
//! a message written for a developer, and per-field messages for invalid
//! input. Messages never carry internal errors, SQL, provider text or secrets.

use std::collections::BTreeMap;

use serde::Serialize;

/// An error the API answers with. The HTTP layer turns it into the envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{status} {code}: {message}")]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub fields: Option<BTreeMap<String, String>>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            fields: None,
        }
    }

    /// No credential, or one that isn't valid.
    pub fn unauthenticated() -> Self {
        Self::new(401, "unauthenticated", "Sign in, or send a valid API key.")
    }

    /// Missing, malformed, outside the caller's organization or workspace: all the same `404`.
    pub fn not_found(what: &str) -> Self {
        Self::new(404, "not_found", format!("{what} was not found."))
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "forbidden", message)
    }

    /// Invalid input, with a message per field.
    pub fn validation(fields: BTreeMap<String, String>) -> Self {
        Self {
            fields: Some(fields),
            ..Self::new(400, "validation_failed", "Some fields are invalid.")
        }
    }

    /// Something went wrong on our side. The cause is logged, never sent.
    pub fn internal() -> Self {
        Self::new(500, "internal_error", "Something went wrong on our side.")
    }

    /// The error as the envelope's `error` field.
    pub fn body(&self) -> ErrorBody<'_> {
        ErrorBody {
            code: self.code,
            message: &self.message,
            fields: self.fields.as_ref(),
        }
    }
}

/// The envelope's `error` field, as the contract's `ApiErrorBody` defines it.
#[derive(Debug, Serialize)]
pub struct ErrorBody<'a> {
    pub code: &'a str,
    pub message: &'a str,
    pub fields: Option<&'a BTreeMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_the_contract_error_body() {
        let error = ApiError::not_found("Workspace");
        let json = serde_json::to_value(error.body()).expect("an error body serializes");
        assert_eq!(
            json,
            serde_json::json!({ "code": "not_found", "message": "Workspace was not found.", "fields": null })
        );
        let fields = BTreeMap::from([("name".to_owned(), "Required.".to_owned())]);
        let json = serde_json::to_value(ApiError::validation(fields).body()).expect("serializes");
        assert_eq!(json["fields"], serde_json::json!({ "name": "Required." }));
    }
}
