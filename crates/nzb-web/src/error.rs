use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::{Serialize, Serializer};

/// Convenience error type for API handlers.
#[derive(Debug)]
pub struct ApiError {
    status: Option<StatusCode>,
    kind: ApiErrorKind,
}

/// Trait for converting Results into ApiErrors with a status code.
pub trait WithStatus<T> {
    fn with_status(self, status: StatusCode) -> Result<T, ApiError>;
}

/// Trait for converting Options into ApiErrors with a status code and message.
pub trait WithStatusError<T> {
    fn with_status_error<E: Into<ApiErrorKind>>(
        self,
        status: StatusCode,
        err: E,
    ) -> Result<T, ApiError>;
}

impl<T> WithStatusError<T> for Option<T> {
    fn with_status_error<E: Into<ApiErrorKind>>(
        self,
        status: StatusCode,
        err: E,
    ) -> Result<T, ApiError> {
        self.ok_or(ApiError {
            status: Some(status),
            kind: err.into(),
        })
    }
}

impl<T, RE> WithStatus<T> for Result<T, RE>
where
    ApiErrorKind: From<RE>,
{
    fn with_status(self, status: StatusCode) -> Result<T, ApiError> {
        self.map_err(|e| ApiError::from((status, ApiErrorKind::from(e))))
    }
}

impl ApiError {
    pub const fn not_found(msg: &'static str) -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::Text(msg),
        }
    }

    pub const fn unauthorized() -> Self {
        Self {
            status: Some(StatusCode::UNAUTHORIZED),
            kind: ApiErrorKind::Unauthorized,
        }
    }

    pub const fn bad_request(msg: &'static str) -> Self {
        Self {
            status: Some(StatusCode::BAD_REQUEST),
            kind: ApiErrorKind::Text(msg),
        }
    }

    pub const fn admission_conflict() -> Self {
        Self {
            status: Some(StatusCode::CONFLICT),
            kind: ApiErrorKind::AdmissionConflict,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ApiErrorKind {
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("server not found: {0}")]
    ServerNotFound(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("idempotency key is already bound to another NZB payload")]
    AdmissionConflict,
    #[error("{0}")]
    Text(&'static str),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    Core(#[from] crate::nzb_core::NzbError),
}

impl From<&'static str> for ApiErrorKind {
    fn from(value: &'static str) -> Self {
        Self::Text(value)
    }
}

impl From<String> for ApiErrorKind {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl Serialize for ApiError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct SerializedError {
            error_kind: &'static str,
            human_readable: String,
            status: u16,
        }

        let serr = SerializedError {
            error_kind: match &self.kind {
                ApiErrorKind::JobNotFound(_)
                | ApiErrorKind::Core(crate::nzb_core::NzbError::JobNotFound(_)) => "job_not_found",
                ApiErrorKind::ServerNotFound(_)
                | ApiErrorKind::Core(crate::nzb_core::NzbError::ServerNotFound(_)) => {
                    "server_not_found"
                }
                ApiErrorKind::Unauthorized => "unauthorized",
                ApiErrorKind::AdmissionConflict
                | ApiErrorKind::Core(crate::nzb_core::NzbError::AdmissionConflict) => {
                    "admission_conflict"
                }
                // Otherwise classify by status so clients can tell a bad
                // request or missing resource from a server fault.
                _ => match self.status() {
                    StatusCode::BAD_REQUEST => "bad_request",
                    StatusCode::NOT_FOUND => "not_found",
                    StatusCode::CONFLICT => "conflict",
                    _ => "internal_error",
                },
            },
            human_readable: format!("{:#}", self.kind),
            status: self.status().as_u16(),
        };
        serr.serialize(serializer)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            status: None,
            kind: ApiErrorKind::Anyhow(value),
        }
    }
}

impl From<crate::nzb_core::NzbError> for ApiError {
    /// Map domain errors to the HTTP status that describes them: missing
    /// resources are 404, malformed input 400, conflicts 409, and only
    /// genuine server-side failures 500.
    fn from(e: crate::nzb_core::NzbError) -> Self {
        use crate::nzb_core::NzbError;
        let status = match &e {
            NzbError::JobNotFound(_)
            | NzbError::ServerNotFound(_)
            | NzbError::CategoryNotFound(_) => StatusCode::NOT_FOUND,
            NzbError::ParseError(_) | NzbError::InvalidNzb(_) => StatusCode::BAD_REQUEST,
            NzbError::AdmissionConflict => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status: Some(status),
            kind: ApiErrorKind::Core(e),
        }
    }
}

impl<E> From<(StatusCode, E)> for ApiError
where
    ApiErrorKind: From<E>,
{
    fn from(value: (StatusCode, E)) -> Self {
        Self {
            status: Some(value.0),
            kind: value.1.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.kind)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Server-side failures (5xx) are otherwise invisible: the error is
        // serialized into the response body but never logged, so operators
        // running at debug level saw only tower_http's "status=500" with no
        // cause (rustnzb#129). Log it here so every 5xx surfaces its
        // underlying error.
        if status.is_server_error() {
            tracing::error!(status = %status, error = %format!("{:#}", self.kind), "API request failed");
        }
        let mut response = axum::Json(&self).into_response();
        *response.status_mut() = status;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nzb_core::NzbError;

    #[test]
    fn domain_errors_map_to_client_statuses() {
        let cases = [
            (NzbError::JobNotFound("j".into()), 404, "job_not_found"),
            (
                NzbError::ServerNotFound("s".into()),
                404,
                "server_not_found",
            ),
            (NzbError::CategoryNotFound("c".into()), 404, "not_found"),
            (NzbError::InvalidNzb("x".into()), 400, "bad_request"),
            (NzbError::ParseError("x".into()), 400, "bad_request"),
            (NzbError::AdmissionConflict, 409, "admission_conflict"),
            (NzbError::Other("boom".into()), 500, "internal_error"),
        ];
        for (error, status, kind) in cases {
            let api = ApiError::from(error);
            assert_eq!(api.status().as_u16(), status);
            let json = serde_json::to_value(&api).unwrap();
            assert_eq!(json["error_kind"], kind);
            assert_eq!(json["status"], status);
        }
    }

    #[test]
    fn text_errors_are_classified_by_status() {
        let json = serde_json::to_value(ApiError::not_found("gone")).unwrap();
        assert_eq!(json["error_kind"], "not_found");
        let json = serde_json::to_value(ApiError::bad_request("bad")).unwrap();
        assert_eq!(json["error_kind"], "bad_request");
    }
}
