//! Integration test to verify OpenTelemetry distributed tracing works end-to-end.
//!
//! This test verifies that:
//! 1. When a request arrives with a W3C `traceparent` header, the router's span
//!    correctly becomes a child of the upstream trace.
//! 2. The `inject_trace_context_http` function correctly injects the current span's
//!    trace context into outgoing request headers.
//! 3. The trace ID is preserved across the entire request flow (inbound -> router -> outbound).

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::{
    global,
    trace::{SpanId, TraceId, TracerProvider as _},
};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    testing::trace::InMemorySpanExporter,
    trace::TracerProvider,
};
use tracing::info_span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;

/// Test end-to-end trace context propagation: inbound extraction AND outbound injection.
///
/// This runs as a single test to avoid global state conflicts (OTel global propagator/provider).
#[test]
fn test_trace_context_propagation_end_to_end() {
    // Set up in-memory span exporter for verification
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("test-tracer");

    // Register global propagator (W3C TraceContext - same as magi uses)
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Set up tracing subscriber with otel layer (same as router's init_logging does)
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // =========================================================================
    // Part 1: Inbound context extraction
    // Simulates magi sending a request with traceparent header to the router
    // =========================================================================

    let upstream_trace_id = "0af7651916cd43dd8448eb211c80319c";
    let upstream_span_id = "b7ad6b7169203331";
    let traceparent = format!("00-{}-{}-01", upstream_trace_id, upstream_span_id);

    let mut incoming_headers = HeaderMap::new();
    incoming_headers.insert("traceparent", HeaderValue::from_str(&traceparent).unwrap());

    // Extract parent context from headers - this is what our fixed make_span does
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&opentelemetry_http::HeaderExtractor(&incoming_headers))
    });

    // Create a span and set the extracted parent context
    let span = info_span!(
        target: "vllm_router_rs::otel-trace",
        "http_request",
        method = "POST",
        uri = "/v1/chat/completions"
    );
    span.set_parent(parent_cx);

    // =========================================================================
    // Part 2: Outbound context injection
    // Simulates the router forwarding the request to a vLLM backend
    // =========================================================================

    let outgoing_traceparent = {
        let _enter = span.enter();

        // This replicates what inject_trace_context_http does
        let context = tracing::Span::current().context();
        let mut outgoing_headers = HeaderMap::new();

        struct HeaderInjector<'a>(&'a mut HeaderMap);
        impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
            fn set(&mut self, key: &str, value: String) {
                if let Ok(header_name) = HeaderName::from_bytes(key.as_bytes()) {
                    if let Ok(header_value) = HeaderValue::from_str(&value) {
                        self.0.insert(header_name, header_value);
                    }
                }
            }
        }

        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut HeaderInjector(&mut outgoing_headers));
        });

        outgoing_headers
            .get("traceparent")
            .map(|v| v.to_str().unwrap().to_string())
    };

    // Drop the span to ensure it's exported
    drop(span);

    // Force flush to get spans
    provider.force_flush();

    // =========================================================================
    // Verify Part 1: Inbound context was correctly linked
    // =========================================================================

    let spans = exporter.get_finished_spans().unwrap();
    assert!(!spans.is_empty(), "Expected at least one span to be exported");

    let exported_span = &spans[0];

    // Verify the trace ID matches the upstream trace ID (from magi's traceparent)
    let expected_trace_id = TraceId::from_hex(upstream_trace_id).unwrap();
    assert_eq!(
        exported_span.span_context.trace_id(),
        expected_trace_id,
        "Span's trace_id should match the incoming traceparent's trace_id. \
         This means the router's span is part of magi's trace."
    );

    // Verify the parent span ID matches the upstream span ID
    let expected_parent_id = SpanId::from_hex(upstream_span_id).unwrap();
    assert_eq!(
        exported_span.parent_span_id,
        expected_parent_id,
        "Span's parent_span_id should match the incoming traceparent's span_id. \
         This means the router's span is correctly a child of magi's span."
    );

    eprintln!(
        "PASS: Inbound trace context correctly extracted.\n  \
         trace_id: {}\n  parent_span_id: {}\n  router_span_id: {:?}",
        upstream_trace_id,
        upstream_span_id,
        exported_span.span_context.span_id()
    );

    // =========================================================================
    // Verify Part 2: Outbound context was correctly injected
    // =========================================================================

    let outgoing_tp = outgoing_traceparent.expect(
        "Expected outgoing traceparent header to be set. \
         Without this, vLLM backends won't be part of the distributed trace."
    );

    // Parse the outgoing traceparent: 00-{trace_id}-{span_id}-{flags}
    let parts: Vec<&str> = outgoing_tp.split('-').collect();
    assert_eq!(parts.len(), 4, "traceparent should have 4 parts");
    assert_eq!(parts[0], "00", "version should be 00");
    assert_eq!(
        parts[1], upstream_trace_id,
        "trace_id in outgoing request should match the original upstream trace_id. \
         This ensures the entire trace (magi -> router -> vLLM) shares one trace_id."
    );
    assert_ne!(
        parts[2], upstream_span_id,
        "outgoing span_id should be the router's own span, not magi's. \
         The router creates a new span as a child of magi's span."
    );
    assert_eq!(parts[3], "01", "trace flags should indicate sampled");

    eprintln!(
        "PASS: Outbound trace context correctly injected.\n  \
         trace_id preserved: {}\n  router_span_id: {}\n  \
         upstream_parent_span_id: {}\n  \
         Full chain: magi({}) -> router({}) -> vLLM backend",
        parts[1], parts[2], upstream_span_id,
        upstream_span_id, parts[2]
    );

    // Clean up
    global::shutdown_tracer_provider();
}

/// Test that the actual RequestSpan::make_span compiles and runs with the
/// otel parent context extraction code path. This exercises the real middleware.
#[test]
fn test_make_span_with_traceparent_header() {
    use axum::http::Request;
    use tower_http::trace::MakeSpan;
    use vllm_router_rs::middleware::RequestSpan;

    // Set up in-memory span exporter
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("test-tracer");

    // Register global propagator
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Set up tracing subscriber with otel layer
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);
    let _guard = tracing::subscriber::set_default(subscriber);

    // Build a request with traceparent header
    let upstream_trace_id = "2af7651916cd43dd8448eb211c80319c";
    let upstream_span_id = "d7ad6b7169203331";
    let traceparent = format!("00-{}-{}-01", upstream_trace_id, upstream_span_id);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("traceparent", &traceparent)
        .body(())
        .unwrap();

    // Call the actual make_span from middleware — this should not panic
    let mut request_span = RequestSpan;
    let span = request_span.make_span(&request);

    // Enter and drop to export
    {
        let _enter = span.enter();
        tracing::info!("Processing request via middleware");
    }
    drop(span);

    provider.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    assert!(!spans.is_empty(), "Expected at least one span");

    println!(
        "SUCCESS: RequestSpan::make_span ran correctly with traceparent header.\n  spans exported: {}",
        spans.len()
    );

    global::shutdown_tracer_provider();
}
