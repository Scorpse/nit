//! 20 sequential cheap GETs against a local mock (loopback only). Ignored by default:
//! `cargo test -p providers --test bench_requests -- --ignored --nocapture`.
use providers::BbClient;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
#[ignore = "timing measurement, not a pass/fail test"]
async fn twenty_sequential_gets() {
    let s = MockServer::start().await;
    let big: Vec<_> = (0..400)
        .map(|i| json!({"id": i, "title": "x".repeat(40)}))
        .collect();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "values": big })))
        .mount(&s)
        .await;
    let c = BbClient::new(&s.uri(), "me@example.com", "t");
    let t = std::time::Instant::now();
    for _ in 0..20 {
        c.get_json("/x").await.unwrap();
    }
    eprintln!("20 sequential GETs: {:?}", t.elapsed());
}
