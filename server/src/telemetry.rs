//! OpenTelemetry export and request correlation identifiers.

use std::fmt;
use std::future::Future;

use anyhow::Result;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::global;
use opentelemetry::propagation::Injector;
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::Identity;
use tracing_subscriber::{EnvFilter, Layer, Registry};
use uuid::Uuid;

use crate::config::{OtlpProtocol, TracingConfig};
use attic::api::{CELLER_OP_ID, REQUEST_ID};

/// Filters what reaches the collector, independently of `RUST_LOG`.
const ENV_OTEL_FILTER: &str = "CELLER_SERVER_OTEL_FILTER";

/// Longest inbound `X-Request-Id` we are willing to echo back.
const MAX_REQUEST_ID_LEN: usize = 128;

/// The instrumentation scope reported for spans we produce.
const INSTRUMENTATION_SCOPE: &str = "cellerd";

/// A boxed layer, never an `Option` of one: `Layer for Option<L>` does not
/// forward `downcast_raw`, which is how `OpenTelemetrySpanExt::context()` finds
/// the OpenTelemetry layer. Behind an `Option` every span context comes back
/// invalid and every op ID degrades to a random UUID.
pub type OtelLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// A server-assigned identifier for a single request.
///
/// Under OTLP export this is the OpenTelemetry trace ID reinterpreted as a
/// UUID, which makes the ID a user quotes directly usable as a trace lookup.
/// Without a live exporter it is a random v4 UUID that still ties a response to
/// its log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(Uuid);

tokio::task_local! {
    static CURRENT_OP_ID: OpId;
}

impl OpId {
    fn from_span(span: &Span) -> Self {
        let context = span.context();
        let span_context = context.span().span_context().clone();

        if span_context.is_valid() {
            Self(Uuid::from_bytes(span_context.trace_id().to_bytes()))
        } else {
            Self(Uuid::new_v4())
        }
    }

    /// Returns the op ID of the request being served on this task.
    pub fn current() -> Option<Self> {
        CURRENT_OP_ID.try_with(|id| *id).ok()
    }

    /// Runs `f` with this op ID installed as the current one.
    pub(crate) async fn scope<F: Future>(self, f: F) -> F::Output {
        CURRENT_OP_ID.scope(self, f).await
    }

    fn header_value(&self) -> HeaderValue {
        let mut buf = Uuid::encode_buffer();
        let encoded = self.0.hyphenated().encode_lower(&mut buf);
        HeaderValue::from_str(encoded).expect("a hyphenated UUID is a valid header value")
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// A client- or proxy-supplied request identifier.
#[derive(Debug, Clone)]
pub struct RequestId(HeaderValue);

impl RequestId {
    /// Adopts an inbound `X-Request-Id`, if it is safe to echo back.
    ///
    /// The value ends up in response headers and log fields, so anything that
    /// is not short, visible ASCII is rejected in favour of a generated ID.
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(REQUEST_ID)?;
        let candidate = value.to_str().ok()?;

        if candidate.is_empty() || candidate.len() > MAX_REQUEST_ID_LEN {
            return None;
        }

        if !candidate.bytes().all(|b| b.is_ascii_graphic()) {
            return None;
        }

        Some(Self(value.clone()))
    }

    fn generate() -> Self {
        let mut buf = Uuid::encode_buffer();
        let encoded = Uuid::new_v4().hyphenated().encode_lower(&mut buf);
        Self(HeaderValue::from_str(encoded).expect("a hyphenated UUID is a valid header value"))
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().unwrap_or_default()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The correlation identifiers attached to a request.
#[derive(Debug, Clone)]
pub struct Correlation {
    pub op_id: OpId,
    pub request_id: RequestId,
}

/// Builds the per-request span that carries the correlation fields.
pub(crate) fn request_span<B>(request: &axum::http::Request<B>) -> Span {
    let method = request.method();
    let path = request.uri().path();

    tracing::info_span!(
        "http_request",
        otel.name = %format_args!("{} {}", method, path),
        otel.kind = "server",
        http.request.method = %method,
        url.path = %path,
        user_agent.original = request
            .headers()
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        http.response.status_code = tracing::field::Empty,
        request_id = tracing::field::Empty,
        op_id = tracing::field::Empty,
        enduser.id = tracing::field::Empty,
        cache_name = tracing::field::Empty,
        store_path_hash = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
}

/// Derives the correlation identifiers for a request and records them on `span`.
pub(crate) fn correlate(span: &Span, headers: &HeaderMap) -> Correlation {
    let request_id = RequestId::from_headers(headers).unwrap_or_else(RequestId::generate);
    let op_id = OpId::from_span(span);

    span.record("request_id", request_id.as_str());
    span.record("op_id", tracing::field::display(op_id));

    Correlation { op_id, request_id }
}

/// Writes the correlation headers onto a response.
pub(crate) fn write_headers(span: &Span, correlation: &Correlation, headers: &mut HeaderMap) {
    headers.insert(CELLER_OP_ID, correlation.op_id.header_value());
    headers.insert(REQUEST_ID, correlation.request_id.0.clone());

    // No-op unless a propagator was installed, which only happens when OTLP
    // export is on and the span context is therefore real.
    let context = span.context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(headers))
    });
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(key.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, value);
        }
    }
}

/// A live OpenTelemetry pipeline.
///
/// Dropping it flushes whatever the batch processor is still holding.
pub struct TelemetryGuard(Option<SdkTracerProvider>);

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.0.take() {
            if let Err(e) = provider.shutdown() {
                eprintln!("Failed to shut down the OpenTelemetry pipeline: {}", e);
            }
        }
    }
}

/// Starts OTLP export, if it is configured, and returns the layer to register.
///
/// Must be called from within the Tokio runtime and before the subscriber is
/// built: a per-layer filter is assigned its filter ID when the subscriber is
/// built, so a layer added afterwards has none and panics on first use.
pub fn init(config: &TracingConfig) -> Result<(OtelLayer, TelemetryGuard)> {
    let otlp = &config.otlp;

    if !otlp.enabled {
        return Ok((Box::new(Identity::new()), TelemetryGuard(None)));
    }

    let exporter = match otlp.protocol {
        OtlpProtocol::Grpc => {
            let mut builder = SpanExporter::builder()
                .with_tonic()
                .with_timeout(otlp.timeout);
            if let Some(endpoint) = &otlp.endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()?
        }
        OtlpProtocol::Http => {
            let mut builder = SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .with_timeout(otlp.timeout);
            if let Some(endpoint) = &otlp.endpoint {
                builder = builder.with_endpoint(endpoint);
            }
            builder.build()?
        }
    };

    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();

    // Parent-based so that a sampling decision made for a trace is honoured for
    // the rest of it, even though we never adopt a caller's trace context.
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(otlp.sample_ratio)));

    let provider = SdkTracerProvider::builder()
        .with_span_processor(BatchSpanProcessor::builder(exporter, Tokio).build())
        .with_resource(resource)
        .with_sampler(sampler)
        .build();

    let tracer = provider.tracer(INSTRUMENTATION_SCOPE);

    global::set_tracer_provider(provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());

    let filter =
        EnvFilter::try_from_env(ENV_OTEL_FILTER).unwrap_or_else(|_| EnvFilter::new("info"));

    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_error_records_to_exceptions(true)
        .with_filter(filter)
        .boxed();

    Ok((layer, TelemetryGuard(Some(provider))))
}
