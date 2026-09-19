// Black-box Typert wire tests — written against spec/typert/, run against any host.
// M0 stub: requires the harness runner to export CONFORMANCE_BASE_URL.

fn base_url() -> String {
    std::env::var("CONFORMANCE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3080".into())
}

/// Signed dsh cookie ("k=v"); ignored by vocoderd.
///
/// Required here for the same reason `endpoints_spec.rs` needs it: the control
/// gates the whole surface behind browser auth, so an unauthenticated `GET /`
/// answers **401**, not the boot document. This cell asserted the boot document
/// and so failed against the control — the identical omission that made the
/// endpoint matrix report 0/5 before it was fixed there.
fn auth_cookie() -> Option<String> {
    let path = std::env::var("CONFORMANCE_COOKIE_FILE").ok()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[tokio::test]
async fn host_serves_boot_document() {
    let mut req = reqwest::Client::new().get(format!("{}/", base_url()));
    if let Some(c) = auth_cookie() {
        req = req.header("cookie", c);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let body = res.text().await.unwrap();
    assert_eq!(status, 200, "the boot document is served: {body:.200}");
    assert!(body.contains("<html"), "boot document should be HTML");
}

// TODO(M1): encode/decode conformance cells per spec/typert endpoint:
//   unary call, malformed-request -> RemoteError(gateway/bad-request),
//   cancellation round-trip, forwarded event subscription.
