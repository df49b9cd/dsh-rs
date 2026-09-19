// Black-box Typert wire tests — written against spec/typert/, run against any host.
// M0 stub: requires the harness runner to export CONFORMANCE_BASE_URL.

fn base_url() -> String {
    std::env::var("CONFORMANCE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into())
}

#[tokio::test]
async fn host_serves_boot_document() {
    let url = base_url();
    let body = reqwest::get(format!("{url}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("<html"), "boot document should be HTML");
}

// TODO(M1): encode/decode conformance cells per spec/typert endpoint:
//   unary call, malformed-request -> RemoteError(gateway/bad-request),
//   cancellation round-trip, forwarded event subscription.
