use axum::http::{HeaderMap, HeaderValue, Request};
use tower_http::trace::MakeSpan;
use vllm_router_rs::{middleware::RequestSpan, otel_trace, routers::header_utils};

#[test]
fn request_span_is_none_when_otel_disabled() {
    otel_trace::shutdown_otel();

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .header(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        )
        .body(())
        .unwrap();

    let mut request_span = RequestSpan;
    let span = request_span.make_span(&request);

    assert!(
        span.is_none(),
        "RequestSpan should return Span::none() when OTel is disabled"
    );
}

#[test]
fn inject_trace_context_http_is_noop_when_otel_disabled() {
    otel_trace::shutdown_otel();

    let mut headers = HeaderMap::new();
    otel_trace::inject_trace_context_http(&mut headers);

    assert!(
        headers.is_empty(),
        "No trace context should be injected when OTel is disabled"
    );
}

#[test]
fn propagate_trace_headers_passively_forwards_only_w3c_headers_when_otel_disabled() {
    otel_trace::shutdown_otel();

    let mut incoming = HeaderMap::new();
    incoming.insert(
        "traceparent",
        HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
    );
    incoming.insert("tracestate", HeaderValue::from_static("vendor=value"));
    incoming.insert("baggage", HeaderValue::from_static("userId=alice"));
    incoming.insert("x-extra-header", HeaderValue::from_static("ignore-me"));

    let request = header_utils::propagate_trace_headers(
        reqwest::Client::new().post("http://example.com"),
        Some(&incoming),
    )
    .build()
    .unwrap();

    assert_eq!(
        request.headers().get("traceparent"),
        incoming.get("traceparent"),
        "traceparent should be forwarded unchanged"
    );
    assert_eq!(
        request.headers().get("tracestate"),
        incoming.get("tracestate"),
        "tracestate should be forwarded unchanged"
    );
    assert_eq!(
        request.headers().get("baggage"),
        incoming.get("baggage"),
        "baggage should be forwarded unchanged"
    );
    assert!(
        request.headers().get("x-extra-header").is_none(),
        "Non-trace headers should not be forwarded by propagate_trace_headers"
    );
}
