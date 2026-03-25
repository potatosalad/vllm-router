mod common;

use axum::{
    body::Body,
    extract::Request,
    http::{header::CONTENT_TYPE, StatusCode},
};
use common::mock_worker::{HealthStatus, MockWorker, MockWorkerConfig, WorkerType};
use common::test_app::create_test_app;
use metrics::set_default_local_recorder;
use metrics_exporter_prometheus::PrometheusBuilder;
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;
use vllm_router_rs::{
    config::{PolicyConfig, RouterConfig, RoutingMode},
    routers::RouterFactory,
};

fn build_test_recorder() -> metrics_exporter_prometheus::PrometheusRecorder {
    PrometheusBuilder::new().build_recorder()
}

fn vllm_pd_router_config(prefill_url: &str, decode_url: &str) -> RouterConfig {
    RouterConfig {
        mode: RoutingMode::VllmPrefillDecode {
            prefill_urls: vec![(prefill_url.to_string(), None)],
            decode_urls: vec![decode_url.to_string()],
            prefill_policy: None,
            decode_policy: None,
            discovery_address: None,
        },
        policy: PolicyConfig::RoundRobin,
        host: "127.0.0.1".to_string(),
        port: 0,
        request_timeout_secs: 10,
        worker_startup_timeout_secs: 5,
        worker_startup_check_interval_secs: 1,
        ..Default::default()
    }
}

fn assert_metric_line(rendered: &str, prefix: &str, fragments: &[&str], suffix: &str) {
    assert!(
        rendered.lines().any(|line| {
            line.starts_with(prefix)
                && fragments.iter().all(|fragment| line.contains(fragment))
                && line.ends_with(suffix)
        }),
        "missing metric line: prefix={prefix}, fragments={fragments:?}, suffix={suffix}\n{rendered}"
    );
}

async fn send_chat_completion_request(
    app: axum::Router,
    prompt: &str,
) -> axum::http::Response<Body> {
    let payload = json!({
        "model": "mock-model",
        "messages": [
            {
                "role": "user",
                "content": prompt,
            }
        ],
        "stream": false,
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    app.oneshot(request).await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn test_vllm_pd_http_status_metrics_cover_prefill_decode_and_final_response() {
    let mut prefill_worker = MockWorker::new(MockWorkerConfig {
        port: 0,
        worker_type: WorkerType::Prefill,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 0,
        fail_rate: 0.0,
    });
    let prefill_url = prefill_worker
        .start()
        .await
        .expect("failed to start prefill worker");

    let mut decode_worker = MockWorker::new(MockWorkerConfig {
        port: 0,
        worker_type: WorkerType::Decode,
        health_status: HealthStatus::Healthy,
        response_delay_ms: 0,
        fail_rate: 0.0,
    });
    let decode_url = decode_worker
        .start()
        .await
        .expect("failed to start decode worker");

    let config = vllm_pd_router_config(&prefill_url, &decode_url);
    let app_context = common::create_test_context(config.clone());
    let router = RouterFactory::create_router(&app_context)
        .await
        .expect("failed to create vLLM PD router");
    let app = create_test_app(Arc::from(router), reqwest::Client::new(), &config);

    let recorder = build_test_recorder();
    let handle = recorder.handle();
    // Use a thread-local recorder so this binary never contends for the global recorder.
    let _recorder_guard = set_default_local_recorder(&recorder);

    let success_response = send_chat_completion_request(app.clone(), "successful decode").await;
    assert_eq!(success_response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(success_response.into_body(), usize::MAX)
        .await
        .unwrap();

    decode_worker
        .set_forced_http_status(Some(StatusCode::TOO_MANY_REQUESTS))
        .await;

    let downstream_error_response =
        send_chat_completion_request(app.clone(), "decode should return 429").await;
    assert_eq!(
        downstream_error_response.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let _ = axum::body::to_bytes(downstream_error_response.into_body(), usize::MAX)
        .await
        .unwrap();

    let rendered = handle.render();
    let route = "route=\"/v1/chat/completions\"";
    let method = "http_request_method=\"POST\"";
    let prefill_worker_label = format!("worker=\"{prefill_url}\"");
    let decode_worker_label = format!("worker=\"{decode_url}\"");

    assert_metric_line(
        &rendered,
        "vllm_router_backend_http_responses_total{",
        &[
            route,
            prefill_worker_label.as_str(),
            "vllm_request_phase=\"prefill\"",
            method,
            "http_response_status_code=\"200\"",
            "http_response_status_class=\"2xx\"",
        ],
        " 2",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_backend_http_response_duration_seconds_count{",
        &[
            route,
            prefill_worker_label.as_str(),
            "vllm_request_phase=\"prefill\"",
            method,
            "http_response_status_code=\"200\"",
            "http_response_status_class=\"2xx\"",
        ],
        " 2",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_backend_http_responses_total{",
        &[
            route,
            decode_worker_label.as_str(),
            "vllm_request_phase=\"decode\"",
            method,
            "http_response_status_code=\"200\"",
            "http_response_status_class=\"2xx\"",
        ],
        " 1",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_backend_http_responses_total{",
        &[
            route,
            decode_worker_label.as_str(),
            "vllm_request_phase=\"decode\"",
            method,
            "http_response_status_code=\"429\"",
            "http_response_status_class=\"4xx\"",
        ],
        " 1",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_backend_http_response_duration_seconds_count{",
        &[
            route,
            decode_worker_label.as_str(),
            "vllm_request_phase=\"decode\"",
            method,
            "http_response_status_code=\"429\"",
            "http_response_status_class=\"4xx\"",
        ],
        " 1",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_http_responses_total{",
        &[
            route,
            method,
            "http_response_status_code=\"200\"",
            "http_response_status_class=\"2xx\"",
        ],
        " 1",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_http_responses_total{",
        &[
            route,
            method,
            "http_response_status_code=\"429\"",
            "http_response_status_class=\"4xx\"",
        ],
        " 1",
    );
    assert_metric_line(
        &rendered,
        "vllm_router_http_response_duration_seconds_count{",
        &[
            route,
            method,
            "http_response_status_code=\"429\"",
            "http_response_status_class=\"4xx\"",
        ],
        " 1",
    );

    decode_worker.set_forced_http_status(None).await;
    prefill_worker.stop().await;
    decode_worker.stop().await;
}
