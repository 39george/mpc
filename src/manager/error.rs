use axum::{
    body::Body,
    response::{IntoResponse, Response},
};
use http::StatusCode;

use crate::impl_debug;

pub type Result<T> = std::result::Result<T, ErrorResponse>;

#[derive(thiserror::Error)]
pub enum ErrorResponse {
    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
    #[error("Internal error")]
    Internal(#[source] anyhow::Error),
    #[error("Bad request")]
    BadRequest(#[source] anyhow::Error),
    #[error("No such account")]
    Unauthorized(#[source] anyhow::Error),
    #[error("Not found error")]
    NotFound(#[source] anyhow::Error),
    #[error("Have no access")]
    Forbidden(#[source] anyhow::Error),
    #[error("Conflict error")]
    Conflict(#[source] anyhow::Error),
    #[error("Unprocessable entity")]
    Unprocessable,
}

impl_debug!(ErrorResponse);

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        tracing::error!("{:?}", self);
        match self {
            ErrorResponse::Unexpected(_) => {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            ErrorResponse::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            // We use middleware to make json response from BadRequest
            ErrorResponse::BadRequest(e) => Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(e.to_string()))
                .unwrap_or(StatusCode::BAD_REQUEST.into_response()),
            ErrorResponse::Unauthorized(_) => {
                StatusCode::UNAUTHORIZED.into_response()
            }
            ErrorResponse::NotFound(err) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/json")
                .body(Body::from(format!("{{\"what\":\"{err}\"}}")))
                .unwrap_or(StatusCode::NOT_FOUND.into_response()),
            ErrorResponse::Forbidden(_) => {
                StatusCode::FORBIDDEN.into_response()
            }
            ErrorResponse::Conflict(e) => Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from(e.to_string()))
                .unwrap_or(StatusCode::CONFLICT.into_response()),
            ErrorResponse::Unprocessable => {
                StatusCode::UNPROCESSABLE_ENTITY.into_response()
            }
        }
    }
}
