use std::convert::Infallible;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Query, Request, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{self, CurrentUser},
    error::ApiError,
    state::AppState,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/System/Endpoint", get(endpoint_info))
        .route("/Playback/BitrateTest", get(bitrate_test))
        .with_state(state)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointInfo {
    is_local: bool,
    is_in_network: bool,
}

async fn endpoint_info(
    State(state): State<AppState>,
    _user: CurrentUser,
    request: Request,
) -> Json<EndpointInfo> {
    let (parts, _) = request.into_parts();
    let address = auth::client_address(&parts, &state.config.trusted_proxies);
    Json(EndpointInfo {
        is_local: address.is_some_and(|address| address.is_loopback()),
        is_in_network: auth::client_is_local(&parts, &state.config),
    })
}

#[derive(Default, Deserialize)]
struct BitrateQuery {
    size: Option<u32>,
}

async fn bitrate_test(
    _user: CurrentUser,
    Query(query): Query<BitrateQuery>,
) -> Result<Response, ApiError> {
    let size = query.size.unwrap_or(102400);
    if !(1..=100_000_000).contains(&size) {
        return Err(ApiError::BadRequest(
            "Bitrate test size must be between 1 and 100000000 bytes".to_owned(),
        ));
    }
    // Reuse a fixed chunk so a large transfer does not allocate its total size.
    const CHUNK: &[u8] = &[0; 64 * 1024];
    let stream = futures_util::stream::unfold(size, |remaining| async move {
        if remaining == 0 {
            return None;
        }
        let length = remaining.min(CHUNK.len() as u32);
        let bytes = Bytes::from_static(CHUNK).slice(..length as usize);
        Some((Ok::<_, Infallible>(bytes), remaining - length))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CACHE_CONTROL, "no-store, no-transform".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
