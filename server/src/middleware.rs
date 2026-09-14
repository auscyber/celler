use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::anyhow;
use axum::{
    extract::{Extension, Request},
    http::HeaderValue,
    middleware::Next,
    response::Response,
};
use tracing::Instrument;

use super::{AuthState, RequestState, RequestStateInner, State};
use crate::error::{ErrorKind, ServerResult};
use crate::telemetry::{self, Correlation};
use attic::api::binary_cache::CELLER_CACHE_VISIBILITY;

fn extract_host(req: &Request) -> Option<String> {
    req.headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
}

/// Attaches the correlation identifiers to the request span and the response.
///
/// Runs inside the span `OtelAxumLayer` opens, so the op ID it derives is that
/// span's OpenTelemetry trace ID — the caller's, when they sent a `traceparent`
/// for us to continue.
pub(crate) async fn correlate_request(req: Request, next: Next) -> Response {
    correlate_inner(req, next)
        .instrument(telemetry::correlation_span())
        .await
}

async fn correlate_inner(mut req: Request, next: Next) -> Response {
    let span = tracing::Span::current();
    let correlation = telemetry::correlate(&span, req.headers());
    let op_id = correlation.op_id;

    req.extensions_mut().insert(correlation.clone());

    let mut response = op_id.scope(next.run(req)).await;

    telemetry::write_headers(&correlation, response.headers_mut());

    response
}

/// Initializes per-request state.
pub async fn init_request_state(
    Extension(state): Extension<State>,
    mut req: Request,
    next: Next,
) -> Response {
    let span = req
        .extensions()
        .get::<Correlation>()
        .map(|correlation| correlation.span.clone())
        .unwrap_or_else(tracing::Span::current);

    let host = extract_host(&req).unwrap_or_default();

    // X-Forwarded-Proto is an untrusted header
    let client_claims_https =
        if let Some(x_forwarded_proto) = req.headers().get("x-forwarded-proto") {
            x_forwarded_proto.as_bytes() == b"https"
        } else {
            false
        };

    let req_state = Arc::new(RequestStateInner {
        auth: AuthState::new(),
        api_endpoint: state.config.api_endpoint.to_owned(),
        substituter_endpoint: state.config.substituter_endpoint.to_owned(),
        host,
        client_claims_https,
        public_cache: AtomicBool::new(false),
        span,
    });

    req.extensions_mut().insert(req_state);
    next.run(req).await
}

/// Restricts valid Host headers.
///
/// We also require that all request have a Host header in
/// the first place.
pub async fn restrict_host(
    Extension(state): Extension<State>,
    req: Request,
    next: Next,
) -> ServerResult<Response> {
    let allowed_hosts = &state.config.allowed_hosts;

    if !allowed_hosts.is_empty() {
        let host = extract_host(&req)
            .ok_or_else(|| ErrorKind::RequestError(anyhow!("Missing Host header")))?;

        if !allowed_hosts.iter().any(|h| h.as_str() == host) {
            return Err(ErrorKind::RequestError(anyhow!("Bad Host")).into());
        }
    }

    Ok(next.run(req).await)
}

/// Sets the `X-Celler-Cache-Visibility` header in responses.
pub(crate) async fn set_visibility_header(
    Extension(req_state): Extension<RequestState>,
    req: Request,
    next: Next,
) -> ServerResult<Response> {
    let mut response = next.run(req).await;

    if req_state.public_cache.load(Ordering::Relaxed) {
        response
            .headers_mut()
            .append(CELLER_CACHE_VISIBILITY, HeaderValue::from_static("public"));
    }

    Ok(response)
}
