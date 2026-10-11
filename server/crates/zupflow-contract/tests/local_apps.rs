//! `SafeHttpClient::for_local_apps` (development only) reaches the local
//! apps it names, over HTTPS with their own certificates, and nothing else.

#![allow(clippy::unwrap_used)]

use zupflow_core::outbound::{Limits, Method, OutboundError, OutboundHttp, OutboundRequest, Refusal};
use zupflow_outbound::{LocalApp, LocalAppError, SafeHttpClient};
use zupflow_taskboard::{Options, start};

fn get(url: &str) -> OutboundRequest {
    OutboundRequest {
        method: Method::Get,
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
        credential: None,
    }
}

fn app(host: &str, port: u16, certificate_pem: &str) -> LocalApp {
    LocalApp {
        host: host.to_owned(),
        port,
        certificate_pem: certificate_pem.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn reaches_the_named_local_apps_and_nothing_else() {
    let taskboard = start(Options::default()).await.unwrap();
    let port = taskboard.port;
    let client = SafeHttpClient::for_local_apps(&[app("taskboard.example", port, &taskboard.certificate_pem)]).unwrap();
    let answer = client
        .send(
            get(&format!(
                "https://taskboard.example:{port}/.well-known/oauth-authorization-server"
            )),
            Limits::default(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status, 200);

    for (url, refusal) in [
        (format!("http://taskboard.example:{port}/"), Refusal::NotHttps),
        (format!("https://127.0.0.1:{port}/"), Refusal::NotPublic),
        (format!("https://[::1]:{port}/"), Refusal::NotPublic),
        (format!("https://localhost:{port}/"), Refusal::NotPublic),
        (format!("https://other.example:{port}/"), Refusal::NotPublic),
        // The trailing dot names the same host to DNS, but isn't the name pinned.
        (format!("https://taskboard.example.:{port}/"), Refusal::NotPublic),
        // Pinned to its port too: another port on loopback isn't the app.
        (
            format!("https://taskboard.example:{}/", port.wrapping_add(1)),
            Refusal::NotPublic,
        ),
        ("https://taskboard.example/".to_owned(), Refusal::NotPublic),
        // Not even a public host: this client reaches the local apps alone.
        ("https://1.1.1.1/".to_owned(), Refusal::NotPublic),
        ("https://api.github.com/".to_owned(), Refusal::NotPublic),
    ] {
        assert_eq!(
            client.send(get(&url), Limits::default()).await,
            Err(OutboundError::Refused(refusal)),
            "{url}"
        );
    }

    // Only the certificate given is trusted: another server on the pinned
    // name and port, with a certificate of its own, isn't reached.
    let impostor = start(Options::default()).await.unwrap();
    let pinned_to_impostor =
        SafeHttpClient::for_local_apps(&[app("taskboard.example", impostor.port, &taskboard.certificate_pem)]).unwrap();
    let refused = pinned_to_impostor
        .send(
            get(&format!("https://taskboard.example:{}/", impostor.port)),
            Limits::default(),
        )
        .await;
    assert!(
        matches!(refused, Err(OutboundError::Failed | OutboundError::Unreachable)),
        "{refused:?}"
    );
}

#[test]
fn takes_only_names_under_the_reserved_example_domain() {
    let pem = "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n";
    for host in [
        "taskboard.com",
        "localhost",
        "taskboard.localhost",
        "Taskboard.example",
        "example",
        "taskboard.example.",
    ] {
        assert_eq!(
            SafeHttpClient::for_local_apps(&[app(host, 7443, pem)]).err(),
            Some(LocalAppError::NotReserved(host.to_owned())),
            "{host}"
        );
    }
    assert_eq!(SafeHttpClient::for_local_apps(&[]).err(), Some(LocalAppError::NoApps));
}
