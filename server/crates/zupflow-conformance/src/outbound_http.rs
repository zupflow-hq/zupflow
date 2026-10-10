//! The `OutboundHttp` suite (decision 0032): what every implementation of
//! the one outbound path does, so a second one can't be looser.
//!
//! - [`refusals`], against the implementation as a deployment runs it:
//!   refused before anything is sent are URLs that aren't absolute HTTPS,
//!   carry a user name or password, or name a non-public address (literal,
//!   or a name that resolves to one); invalid headers; and a credential
//!   that's empty, over the cap or in a header only the path sets.
//! - [`answers`], against the same implementation configured to reach
//!   plain HTTP on loopback (which only tests can ask for), and a server the
//!   suite starts there: the credential is sent once, last, over a caller's
//!   header of the same name, and scrubbed from the body and headers that
//!   come back, as a whole value and as its bare token; a header named after
//!   it is dropped; `Host` and `Accept-Encoding` from the caller are never
//!   sent; redirects are refused, not followed; a body over the limit and a
//!   call over the time limit fail; any other status is an answer; a closed
//!   port is unreachable.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use zeroize_free::credential;
use zupflow_core::outbound::{
    Limits, MAX_CREDENTIAL_BYTES, Method, OutboundError, OutboundHttp, OutboundRequest, Refusal,
};

/// `Credential` holds its value in a `Zeroizing<String>`; built here so the
/// suite needs no dependency of its own for it.
mod zeroize_free {
    use zupflow_core::outbound::Credential;

    pub fn credential(header: &str, value: &str) -> Credential {
        Credential {
            header: header.to_owned(),
            value: value.to_owned().into(),
        }
    }
}

fn request(method: Method, url: &str) -> OutboundRequest {
    OutboundRequest {
        method,
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
        credential: None,
    }
}

/// Refusals before sending, by the implementation as deployed.
pub async fn refusals(client: &dyn OutboundHttp) {
    let refused = [
        ("not a url", Refusal::InvalidUrl),
        ("/relative", Refusal::InvalidUrl),
        ("https://user:secret@api.example/", Refusal::InvalidUrl),
        ("http://api.example/", Refusal::NotHttps),
        ("ftp://api.example/", Refusal::NotHttps),
        ("https://10.0.0.1/", Refusal::NotPublic),
        ("https://127.0.0.1:8080/", Refusal::NotPublic),
        ("https://[::1]/", Refusal::NotPublic),
        ("https://169.254.169.254/latest/meta-data/", Refusal::NotPublic),
        ("https://[::ffff:127.0.0.1]/", Refusal::NotPublic),
        ("https://[fd00::1]/", Refusal::NotPublic),
        ("https://[64:ff9b::a00:1]/", Refusal::NotPublic),
        ("https://2130706433/", Refusal::NotPublic),
        ("https://127.1/", Refusal::NotPublic),
        // Names are judged by what they resolve to.
        ("https://localhost:9/", Refusal::NotPublic),
    ];
    for (url, refusal) in refused {
        assert_eq!(
            client.send(request(Method::Get, url), Limits::default()).await,
            Err(OutboundError::Refused(refusal)),
            "{url}"
        );
    }

    let with_credential = |header: &str, value: &str| {
        let mut call = request(Method::Get, "https://api.example/");
        call.credential = Some(credential(header, value));
        call
    };
    let mut bad_name = request(Method::Get, "https://api.example/");
    bad_name.headers.push(("bad header".to_owned(), "x".to_owned()));
    for (what, call) in [
        ("a header name that isn't one", bad_name),
        (
            "a credential over the cap",
            with_credential("authorization", &"x".repeat(MAX_CREDENTIAL_BYTES + 1)),
        ),
        ("an empty credential", with_credential("authorization", "")),
        ("a credential as Host", with_credential("Host", "tok_test_s3cret0123")),
        (
            "a credential as Content-Length",
            with_credential("content-length", "tok_test_s3cret0123"),
        ),
    ] {
        assert_eq!(
            client.send(call, Limits::default()).await,
            Err(OutboundError::Refused(Refusal::InvalidHeader)),
            "refuses {what}"
        );
    }
}

/// A server on loopback answering the suite's requests; its base URL, and
/// the `Authorization` values it was sent.
async fn server() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let app = axum::Router::new()
        .route(
            "/echo",
            post(move |headers: HeaderMap, body: String| async move {
                let all: Vec<String> = headers
                    .get_all("authorization")
                    .iter()
                    .map(|value| value.to_str().unwrap_or_default().to_owned())
                    .collect();
                recorded.lock().unwrap().extend(all);
                let authorization = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let token = authorization.rsplit(' ').next().unwrap_or_default().to_owned();
                let host = headers
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let mut answer = HeaderMap::new();
                for (name, value) in [
                    ("x-echo".to_owned(), format!("you sent {authorization}")),
                    (format!("x-{}", token.to_ascii_lowercase()), "named after it".to_owned()),
                    ("x-host".to_owned(), host),
                    (
                        "x-encoding".to_owned(),
                        headers.contains_key("accept-encoding").to_string(),
                    ),
                ] {
                    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                        answer.insert(name, value);
                    }
                }
                (answer, format!("{body} / {authorization} / just {token}"))
            }),
        )
        .route(
            "/redirect",
            get(|| async { (StatusCode::FOUND, [("location", "https://elsewhere.example/")]) }),
        )
        .route(
            "/permanent",
            get(|| async {
                (
                    StatusCode::PERMANENT_REDIRECT,
                    [("location", "https://elsewhere.example/")],
                )
            }),
        )
        .route("/big", get(|| async { vec![b'x'; 2 << 20] }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "late"
            }),
        )
        .route("/not-modified", get(|| async { StatusCode::NOT_MODIFIED }))
        .route(
            "/error",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "the provider's own words").into_response() }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (base, seen)
}

/// What comes back, by the implementation configured to reach plain HTTP on
/// loopback.
pub async fn answers(client: &dyn OutboundHttp) {
    let (base, seen) = server().await;
    let mut call = request(Method::Post, &format!("{base}/echo"));
    call.body = Some(b"hello".to_vec());
    call.headers
        .push(("authorization".to_owned(), "Bearer not-the-credential".to_owned()));
    call.headers.push(("host".to_owned(), "internal.example".to_owned()));
    call.headers.push(("accept-encoding".to_owned(), "gzip".to_owned()));
    call.credential = Some(credential("authorization", "Bearer tok_test_s3cret0123"));
    let response = client.send(call, Limits::default()).await.unwrap();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["Bearer tok_test_s3cret0123"],
        "the credential is sent once, over the caller's header of the same name"
    );
    assert_eq!(response.status, 200);
    assert_eq!(
        String::from_utf8(response.body.clone()).unwrap(),
        "hello / [redacted] / just [redacted]",
        "scrubbed from the body, whole and as its bare token"
    );
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(header("x-echo"), Some("you sent [redacted]"), "scrubbed from headers");
    assert!(
        response
            .headers
            .iter()
            .all(|(name, value)| !name.contains("s3cret") && !value.contains("s3cret")),
        "a header named after the credential is dropped"
    );
    assert!(
        header("x-host").unwrap_or_default().starts_with("127.0.0.1"),
        "the caller's Host isn't sent"
    );
    assert_eq!(header("x-encoding"), Some("false"), "nor its Accept-Encoding");

    let get = |path: &str| request(Method::Get, &format!("{base}{path}"));
    for redirect in ["/redirect", "/permanent"] {
        assert_eq!(
            client.send(get(redirect), Limits::default()).await,
            Err(OutboundError::Redirected),
            "a redirect is refused, not followed: {redirect}"
        );
    }
    assert_eq!(
        client.send(get("/big"), Limits::default()).await,
        Err(OutboundError::TooLarge),
        "a body over the limit"
    );
    let small = Limits {
        max_response_bytes: 4,
        ..Limits::default()
    };
    assert_eq!(
        client
            .send(get("/not-modified"), small)
            .await
            .map(|answer| answer.status),
        Ok(304),
        "a 304 is an answer, not a redirect"
    );
    let quick = Limits {
        timeout: Duration::from_millis(300),
        ..Limits::default()
    };
    assert_eq!(
        client.send(get("/slow"), quick).await,
        Err(OutboundError::TimedOut),
        "a call over the time limit"
    );
    let error = client.send(get("/error"), Limits::default()).await.unwrap();
    assert_eq!(error.status, 500, "any status is an answer for the caller to classify");
    // A port nothing listens on: bound, then released.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    assert_eq!(
        client
            .send(
                request(Method::Get, &format!("http://127.0.0.1:{closed}/")),
                Limits::default()
            )
            .await,
        Err(OutboundError::Unreachable)
    );
}
