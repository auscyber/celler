pub mod binary_cache;
pub mod v1;

/// Response header carrying the server-assigned operation ID of a request.
///
/// When the server exports to OTLP this is the OpenTelemetry trace ID, so an
/// op ID quoted out of an error message resolves directly to a trace.
pub const CELLER_OP_ID: &str = "X-Celler-Op-Id";

/// Request and response header carrying a client- or proxy-supplied request ID.
///
/// The server adopts an inbound value when it is safe to echo, and generates
/// one otherwise.
pub const REQUEST_ID: &str = "X-Request-Id";
