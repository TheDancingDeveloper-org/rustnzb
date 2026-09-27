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
                ApiErrorKind::JobNotFound(_) => "job_not_found",
                ApiErrorKind::ServerNotFound(_) => "server_not_found",
                ApiErrorKind::Unauthorized => "unauthorized",
                ApiErrorKind::AdmissionConflict => "admission_conflict",
                _ => "internal_error",
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
    fn from(e: crate::nzb_core::NzbError) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
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
