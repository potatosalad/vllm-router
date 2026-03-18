//! Integration tests for OpenTelemetry distributed tracing.
//!
//! These tests verify that:
//! 1. Inbound `traceparent` is extracted into the router span and the router
//!    span becomes a child of the upstream caller's span.
//! 2. `inject_trace_context_http` (the real crate helper) correctly injects
//!    the router's span context into outgoing backend requests.
//! 3. Inbound `baggage` survives the enabled path via the composite propagator.
//!
//! NOTE: These tests use global OTel state (`global::set_text_map_propagator`,
//! `global::shutdown_tracer_provider`, and the `ENABLED` flag). They are safe
//! today because integration tests compile into separate binaries, but they
//! will become flaky if moved into the library's unit test suite or if
//! `--test-threads` parallelism is increased within this binary. Keep each
//! test self-contained and ensure cleanup at the end.

use axum::http::{HeaderMap, HeaderValue};
use opentelemetry::{
    global,
    propagation::TextMapCompositePropagator,
    trace::{SpanId, TraceId, TracerProvider as _},
};
use opentelemetry_sdk::{
    propagation::{BaggagePropagator, TraceContextPropagator},
    testing::trace::InMemorySpanExporter,
    trace::TracerProvider,
};
use tracing::info_span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use vllm_router_rs::otel_trace;

/// Helper: set up a test OTel environment with in-memory exporter.
/// Returns the exporter, provider, and a subscriber guard.
fn setup_test_otel() -> (
    InMemorySpanExporter,
    TracerProvider,
    tracing::subscriber::DefaultGuard,
) {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("test-tracer");

    // Use the same composite propagator as production code
    global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));

    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);
    let guard = tracing::subscriber::set_default(subscriber);

    // Enable the flag so real code paths (make_span, inject_trace_context_http) activate
    otel_trace::mark_otel_enabled();

    (exporter, provider, guard)
}

/// Helper: tear down the test OTel environment.
fn teardown_test_otel() {
    otel_trace::shutdown_otel();
}

/// Test end-to-end trace context propagation using the real crate helpers:
/// inbound extraction via make_span + outbound injection via inject_trace_context_http.
#[test]
fn test_trace_context_propagation_end_to_end() {
    let (exporter, provider, _guard) = setup_test_otel();

    // =========================================================================
    // Part 1: Inbound context extraction
    // =========================================================================

    let upstream_trace_id = "0af7651916cd43dd8448eb211c80319c";
    let upstream_span_id = "b7ad6b7169203331";
    let traceparent = format!("00-{}-{}-01", upstream_trace_id, upstream_span_id);

    let mut incoming_headers = HeaderMap::new();
    incoming_headers.insert("traceparent", HeaderValue::from_str(&traceparent).unwrap());

    // Extract parent context from headers (same as make_span does)
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&opentelemetry_http::HeaderExtractor(&incoming_headers))
    });

    let span = info_span!(
        target: "vllm_router_rs::otel-trace",
        "http_request",
        method = "POST",
        uri = "/v1/chat/completions"
    );
    span.set_parent(parent_cx);

    // =========================================================================
    // Part 2: Outbound context injection using the REAL crate helper
    // =========================================================================

    let outgoing_headers = {
        let _enter = span.enter();

        let mut headers = HeaderMap::new();
        otel_trace::inject_trace_context_http(&mut headers);
        headers
    };

    drop(span);
    provider.force_flush();

    // =========================================================================
    // Verify Part 1: Inbound context was correctly linked
    // =========================================================================

    let spans = exporter.get_finished_spans().unwrap();
    assert!(!spans.is_empty(), "Expected at least one span to be exported");

    let exported_span = &spans[0];
    let expected_trace_id = TraceId::from_hex(upstream_trace_id).unwrap();
    assert_eq!(
        exported_span.span_context.trace_id(),
        expected_trace_id,
        "Span's trace_id should match the incoming traceparent's trace_id"
    );

    let expected_parent_id = SpanId::from_hex(upstream_span_id).unwrap();
    assert_eq!(
        exported_span.parent_span_id, expected_parent_id,
        "Span's parent_span_id should match the incoming traceparent's span_id"
    );

    // =========================================================================
    // Verify Part 2: Outbound context was correctly injected
    // =========================================================================

    let outgoing_tp = outgoing_headers
        .get("traceparent")
        .expect("Expected outgoing traceparent header")
        .to_str()
        .unwrap();

    let parts: Vec<&str> = outgoing_tp.split('-').collect();
    assert_eq!(parts.len(), 4, "traceparent should have 4 parts");
    assert_eq!(parts[0], "00", "version should be 00");
    assert_eq!(
        parts[1], upstream_trace_id,
        "trace_id in outgoing request should match the original upstream trace_id"
    );
    assert_ne!(
        parts[2], upstream_span_id,
        "outgoing span_id should be the router's own span, not the upstream's"
    );
    assert_eq!(parts[3], "01", "trace flags should indicate sampled");

    teardown_test_otel();
}

/// Test that the actual RequestSpan::make_span exercises the parent
/// extraction path when OTel is enabled.
#[test]
fn test_make_span_with_traceparent_header() {
    use axum::http::Request;
    use tower_http::trace::MakeSpan;
    use vllm_router_rs::middleware::RequestSpan;

    let (exporter, provider, _guard) = setup_test_otel();

    let upstream_trace_id = "2af7651916cd43dd8448eb211c80319c";
    let upstream_span_id = "d7ad6b7169203331";
    let traceparent = format!("00-{}-{}-01", upstream_trace_id, upstream_span_id);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("traceparent", &traceparent)
        .body(())
        .unwrap();

    // Call the actual make_span from middleware with OTel enabled
    let mut request_span = RequestSpan;
    let span = request_span.make_span(&request);

    {
        let _enter = span.enter();
    }
    drop(span);

    provider.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    assert!(!spans.is_empty(), "Expected at least one span");

    // Verify parent context was extracted (this was previously skipped!)
    let exported_span = &spans[0];
    let expected_trace_id = TraceId::from_hex(upstream_trace_id).unwrap();
    assert_eq!(
        exported_span.span_context.trace_id(),
        expected_trace_id,
        "make_span should extract the upstream trace_id from traceparent"
    );

    let expected_parent_id = SpanId::from_hex(upstream_span_id).unwrap();
    assert_eq!(
        exported_span.parent_span_id, expected_parent_id,
        "make_span should set the upstream span as parent"
    );

    teardown_test_otel();
}

/// Regression test: inbound `baggage` header survives the enabled path.
#[test]
fn test_baggage_survives_enabled_path() {
    let (_exporter, _provider, _guard) = setup_test_otel();

    let mut incoming_headers = HeaderMap::new();
    incoming_headers.insert(
        "traceparent",
        HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
    );
    incoming_headers.insert(
        "baggage",
        HeaderValue::from_static("userId=alice,serverNode=DF%2028"),
    );

    // Extract context (includes baggage via composite propagator)
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&opentelemetry_http::HeaderExtractor(&incoming_headers))
    });

    let span = info_span!(target: "vllm_router_rs::otel-trace", "http_request");
    span.set_parent(parent_cx);

    // Inject outbound — should include baggage
    let outgoing_headers = {
        let _enter = span.enter();
        let mut headers = HeaderMap::new();
        otel_trace::inject_trace_context_http(&mut headers);
        headers
    };

    let baggage = outgoing_headers
        .get("baggage")
        .expect("baggage header should be propagated when tracing is enabled")
        .to_str()
        .unwrap();

    assert!(
        baggage.contains("userId=alice"),
        "baggage should contain userId=alice, got: {baggage}"
    );

    teardown_test_otel();
}
