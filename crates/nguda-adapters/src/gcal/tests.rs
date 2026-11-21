//! The gcal adapter against a wiremock server: sign-in through the loopback
//! redirect, refresh, events parsing and paging, and the GET-only rule.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use nguda_core::SecretStoreError;
use serde_json::json;
use wiremock::matchers::query_param_is_missing;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// An in-memory secret store.
#[derive(Default)]
struct MemorySecrets(Mutex<HashMap<String, String>>);

impl MemorySecrets {
    fn with_refresh(token: &str) -> Arc<Self> {
        let s = Self::default();
        s.0.lock()
            .unwrap()
            .insert(REFRESH_TOKEN_KEY.into(), token.into());
        Arc::new(s)
    }

    fn refresh(&self) -> Option<String> {
        self.0.lock().unwrap().get(REFRESH_TOKEN_KEY).cloned()
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// A secret store whose every call fails, like a locked or missing keyring.
struct BrokenSecrets;

impl SecretStore for BrokenSecrets {
    fn get(&self, _key: &str) -> Result<Option<String>, SecretStoreError> {
        Err(SecretStoreError::new(
            "the keyring is locked or not accessible",
        ))
    }

    fn set(&self, _key: &str, _value: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::new(
            "the keyring is locked or not accessible",
        ))
    }

    fn delete(&self, _key: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::new(
            "the keyring is locked or not accessible",
        ))
    }
}

fn client() -> Option<(String, String)> {
    Some(("client-id".into(), "client-secret".into()))
}

fn config(server: &MockServer) -> GcalConfig {
    GcalConfig::with_base(&server.uri(), client(), Duration::from_secs(5))
}

/// An open-url callback that never opens anything.
fn no_browser() -> OpenUrl {
    Arc::new(|_| Ok(()))
}

/// An open-url callback acting as the browser: it records the auth URL and hits
/// the loopback redirect with `code` and the returned state (or `state` when
/// given), from a plain thread.
fn browser(seen: Arc<Mutex<Option<Url>>>, state: Option<&'static str>) -> OpenUrl {
    Arc::new(move |url: &str| {
        let url = Url::parse(url).map_err(|e| e.to_string())?;
        *seen.lock().unwrap() = Some(url.clone());
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
                .unwrap()
        };
        let redirect = Url::parse(&param("redirect_uri")).unwrap();
        let returned = state.map_or_else(|| param("state"), str::to_owned);
        let addr = format!("127.0.0.1:{}", redirect.port().unwrap());
        std::thread::spawn(move || {
            // An idle connection held open throughout, like a browser preconnect:
            // it must not block the redirect behind it.
            let _idle = std::net::TcpStream::connect(&addr).unwrap();
            // A stray request first, like a browser's favicon fetch.
            let mut favicon = std::net::TcpStream::connect(&addr).unwrap();
            favicon
                .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut sink = Vec::new();
            let _ = favicon.read_to_end(&mut sink);
            let mut s = std::net::TcpStream::connect(&addr).unwrap();
            let request =
                format!("GET /?state={returned}&code=the-code&scope=x HTTP/1.1\r\nHost: x\r\n\r\n");
            s.write_all(request.as_bytes()).unwrap();
            let mut page = String::new();
            let _ = s.read_to_string(&mut page);
        });
        Ok(())
    })
}

fn token_json(refresh: Option<&str>, access: &str) -> serde_json::Value {
    let mut body = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": 3600,
    });
    if let Some(r) = refresh {
        body["refresh_token"] = json!(r);
    }
    body
}

async fn mount_primary(server: &MockServer, id: &str) {
    Mock::given(method("GET"))
        .and(path("/calendar/v3/users/me/calendarList/primary"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": id })))
        .mount(server)
        .await;
}

fn timed(id: &str, title: &str, start: &str, end: &str) -> serde_json::Value {
    json!({
        "id": id,
        "status": "confirmed",
        "summary": title,
        "start": { "dateTime": start },
        "end": { "dateTime": end },
    })
}

fn window() -> (Timestamp, Timestamp) {
    (
        "2026-11-21T00:00:00Z".parse().unwrap(),
        "2026-11-22T00:00:00Z".parse().unwrap(),
    )
}

fn assert_get_only(requests: &[wiremock::Request]) {
    for r in requests {
        if r.url.path().starts_with("/calendar/v3") {
            assert_eq!(r.method.as_str(), "GET", "{}", r.url.path());
        } else {
            assert_eq!(r.url.path(), "/token");
            assert_eq!(r.method.as_str(), "POST");
        }
    }
}

#[tokio::test]
async fn sign_in_exchanges_the_code_and_stores_the_refresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .expect(1)
        .mount(&server)
        .await;
    mount_primary(&server, "me@example.com").await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/me@example.com/events"))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    let secrets = Arc::new(MemorySecrets::default());
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        secrets.clone(),
        browser(seen.clone(), None),
    );

    source.sign_in().await.unwrap();
    assert_eq!(secrets.refresh().as_deref(), Some("rt-1"));

    let auth_url = seen.lock().unwrap().clone().unwrap();
    assert_eq!(auth_url.path(), "/o/oauth2/v2/auth");
    let params: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
    assert_eq!(params["scope"], SCOPES.join(" "));
    assert_eq!(params["access_type"], "offline");
    assert_eq!(params["prompt"], "consent");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["response_type"], "code");
    assert!(params["redirect_uri"].starts_with("http://127.0.0.1:"));
    assert!(!params["state"].is_empty());

    // The access token from the exchange is reused: no refresh happens.
    let (from, to) = window();
    assert_eq!(source.fetch_primary(from, to).await.unwrap(), vec![]);
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn a_wrong_state_is_rejected_without_a_token_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt"), "at")))
        .expect(0)
        .mount(&server)
        .await;
    let secrets = Arc::new(MemorySecrets::default());
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        secrets.clone(),
        browser(seen, Some("forged")),
    );
    let err = source.sign_in().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Auth);
    assert_eq!(err.message, "sign-in state mismatch");
    assert_eq!(secrets.refresh(), None);
}

#[tokio::test]
async fn sign_in_times_out_when_the_browser_never_comes_back() {
    let server = MockServer::start().await;
    let config = GcalConfig::with_base(&server.uri(), client(), Duration::from_millis(50));
    let source = GcalSource::new(config, Arc::new(MemorySecrets::default()), no_browser());
    let err = source.sign_in().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Auth);
    assert_eq!(err.message, "sign-in timed out");
}

#[tokio::test]
async fn a_token_error_carries_the_oauth_code_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "secret detail rt-9",
        })))
        .mount(&server)
        .await;
    let source = GcalSource::new(
        config(&server),
        MemorySecrets::with_refresh("rt-9"),
        no_browser(),
    );
    let (from, to) = window();
    let err = source.fetch_primary(from, to).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Auth);
    assert!(err.message.contains("invalid_grant"), "{err}");
    assert!(!err.message.contains("rt-9"), "{err}");
}

#[tokio::test]
async fn no_built_in_client_fails_sign_in_without_opening_a_browser() {
    let server = MockServer::start().await;
    let config = GcalConfig::with_base(&server.uri(), None, Duration::from_secs(1));
    let opened = Arc::new(Mutex::new(false));
    let flag = opened.clone();
    let open: OpenUrl = Arc::new(move |_| {
        *flag.lock().unwrap() = true;
        Ok(())
    });
    let source = GcalSource::new(config, Arc::new(MemorySecrets::default()), open);
    let err = source.sign_in().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NoClient);
    assert_eq!(err.message, "no Google OAuth client built in");
    assert!(!*opened.lock().unwrap());
}

#[tokio::test]
async fn fetch_without_a_refresh_token_is_not_signed_in() {
    let server = MockServer::start().await;
    let source = GcalSource::new(
        config(&server),
        Arc::new(MemorySecrets::default()),
        no_browser(),
    );
    let (from, to) = window();
    let err = source.fetch_primary(from, to).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fetch_refreshes_then_parses_and_pages_the_primary_events() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=rt-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(None, "at-2")))
        .expect(1)
        .mount(&server)
        .await;
    mount_primary(&server, "me@example.com").await;
    let events = "/calendar/v3/calendars/me@example.com/events";
    let page1 = json!({
        "items": [
            timed("a", "Standup", "2026-11-21T09:00:00+01:00", "2026-11-21T09:15:00+01:00"),
            {
                "id": "allday", "status": "confirmed", "summary": "Holiday",
                "start": { "date": "2026-11-21" }, "end": { "date": "2026-11-22" },
            },
            {
                "id": "declined", "status": "confirmed", "summary": "Nope",
                "start": { "dateTime": "2026-11-21T11:00:00Z" },
                "end": { "dateTime": "2026-11-21T12:00:00Z" },
                "attendees": [
                    { "email": "x@example.com", "responseStatus": "accepted" },
                    { "email": "me@example.com", "self": true, "responseStatus": "declined" },
                ],
            },
            { "id": "gone", "status": "cancelled" },
            timed("empty", "Zero", "2026-11-21T13:00:00Z", "2026-11-21T13:00:00Z"),
            timed("overnight", "Late", "2026-11-20T23:00:00Z", "2026-11-21T01:00:00Z"),
        ],
        "nextPageToken": "p2",
    });
    let page2 = json!({
        "items": [
            {
                "id": "weekly_20261121T140000Z", "status": "confirmed",
                "start": { "dateTime": "2026-11-21T15:00:00Z" },
                "end": { "dateTime": "2026-11-21T15:30:00Z" },
                "originalStartTime": { "dateTime": "2026-11-21T14:00:00Z" },
                "attendees": [
                    { "email": "me@example.com", "self": true, "responseStatus": "accepted" },
                ],
            },
        ],
    });
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("timeMin", "2026-11-21T00:00:00Z"))
        .and(query_param("timeMax", "2026-11-22T00:00:00Z"))
        .and(query_param("singleEvents", "true"))
        .and(query_param("orderBy", "startTime"))
        .and(query_param_is_missing("pageToken"))
        .and(header("authorization", "Bearer at-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page1))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page2))
        .mount(&server)
        .await;

    let source = GcalSource::new(
        config(&server),
        MemorySecrets::with_refresh("rt-1"),
        no_browser(),
    );
    let (from, to) = window();
    let got = source.fetch_primary(from, to).await.unwrap();
    let ts = |s: &str| s.parse::<Timestamp>().unwrap();
    assert_eq!(
        got,
        vec![
            FetchedEvent {
                calendar_id: "me@example.com".into(),
                source_id: "a".into(),
                instance_start: None,
                title: "Standup".into(),
                start: ts("2026-11-21T08:00:00Z"),
                end: ts("2026-11-21T08:15:00Z"),
            },
            FetchedEvent {
                calendar_id: "me@example.com".into(),
                source_id: "weekly_20261121T140000Z".into(),
                instance_start: Some(ts("2026-11-21T14:00:00Z")),
                title: "(no title)".into(),
                start: ts("2026-11-21T15:00:00Z"),
                end: ts("2026-11-21T15:30:00Z"),
            },
        ]
    );
    // A second fetch reuses the refreshed access token (the token mock expects 1).
    source.fetch_primary(from, to).await.unwrap();
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn an_http_failure_is_an_other_error_without_details() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(None, "at")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Standup details"))
        .mount(&server)
        .await;
    let source = GcalSource::new(
        config(&server),
        MemorySecrets::with_refresh("rt"),
        no_browser(),
    );
    let (from, to) = window();
    let err = source.fetch_primary(from, to).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Other);
    assert_eq!(err.message, "Calendar API HTTP 503");
}

#[test]
fn calendar_ids_are_percent_encoded_in_the_path() {
    let source = GcalSource::new(
        GcalConfig::with_base("http://127.0.0.1:1", client(), Duration::from_secs(1)),
        Arc::new(MemorySecrets::default()),
        no_browser(),
    );
    let url = source
        .inner
        .api_url(&["calendars", "a/b#c?d e", "events"])
        .unwrap();
    assert_eq!(
        url.path(),
        "/calendar/v3/calendars/a%2Fb%23c%3Fd%20e/events"
    );
}

#[test]
fn the_config_debug_never_prints_the_secret() {
    let c = GcalConfig::with_base("http://x", client(), Duration::from_secs(1));
    let printed = format!("{c:?}");
    assert!(printed.contains("client-id"));
    assert!(!printed.contains("client-secret"));
}

#[tokio::test]
async fn an_unreadable_keyring_is_not_signed_in() {
    let server = MockServer::start().await;
    let source = GcalSource::new(config(&server), Arc::new(BrokenSecrets), no_browser());
    let (from, to) = window();
    let err = source.fetch_primary(from, to).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_sign_in_that_cannot_be_stored_leaves_no_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .expect(1)
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        Arc::new(BrokenSecrets),
        browser(seen, None),
    );
    let err = source.sign_in().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Other);
    // The exchanged access token was not kept: a fetch is not signed in.
    let (from, to) = window();
    let err = source.fetch_primary(from, to).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
}
