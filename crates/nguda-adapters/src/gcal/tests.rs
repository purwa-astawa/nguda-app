//! The gcal adapter against a wiremock server: sign-in through the loopback
//! redirect, refresh, events parsing and paging, and the GET-only rule.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;

use nguda_core::SecretStoreError;
use serde_json::json;
use wiremock::matchers::{
    body_string_contains, header, method, path, query_param, query_param_is_missing,
};
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

/// The error message of every [`BrokenSecrets`] call.
const LOCKED: &str = "the keyring is locked or not accessible";

/// A secret store whose every call fails, like a locked or missing keyring.
struct BrokenSecrets;

impl SecretStore for BrokenSecrets {
    fn get(&self, _key: &str) -> Result<Option<String>, SecretStoreError> {
        Err(SecretStoreError::new(LOCKED))
    }

    fn set(&self, _key: &str, _value: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::new(LOCKED))
    }

    fn delete(&self, _key: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::new(LOCKED))
    }
}

fn client() -> Option<OAuthClient> {
    Some(OAuthClient {
        id: "client-id".into(),
        secret: Some("client-secret".into()),
    })
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
    gated_browser(seen, state, None)
}

/// Like [`browser`], but with a `gate`, the redirect waits until the gate gets a
/// message (or its sender is dropped).
fn gated_browser(
    seen: Arc<Mutex<Option<Url>>>,
    state: Option<&'static str>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> OpenUrl {
    let gate = Arc::new(Mutex::new(gate));
    Arc::new(move |url: &str| {
        let gate = gate.lock().unwrap().take();
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
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
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

/// Syncs `me@example.com`, fully when `token` is `None`.
async fn sync_me(source: &GcalSource, token: Option<&str>) -> Result<SyncBatch, CalendarError> {
    let (from, to) = window();
    source
        .sync_calendar("me@example.com".into(), token.map(str::to_owned), from, to)
        .await
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
    assert_eq!(sync_me(&source, None).await.unwrap().changes, vec![]);
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
            "error": "invalid_client",
            "error_description": "secret detail rt-9",
        })))
        .mount(&server)
        .await;
    let secrets = MemorySecrets::with_refresh("rt-9");
    let source = GcalSource::new(config(&server), secrets.clone(), no_browser());
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Auth);
    // Only invalid_grant ends the session: the token stays.
    assert_eq!(secrets.refresh().as_deref(), Some("rt-9"));
    assert!(err.message.contains("invalid_client"), "{err}");
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
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fetch_refreshes_then_parses_and_pages_a_calendars_events() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=rt-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(None, "at-2")))
        .expect(1)
        .mount(&server)
        .await;
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
            timed("tomorrow", "Next", "2026-11-22T00:00:00Z", "2026-11-22T01:00:00Z"),
        ],
        "nextPageToken": "p2",
        "nextSyncToken": "not-the-last-page",
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
        "nextSyncToken": "sync-1",
    });
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("timeMin", "2026-11-21T00:00:00Z"))
        .and(query_param("timeMax", "2026-11-22T00:00:00Z"))
        .and(query_param("singleEvents", "true"))
        .and(query_param("showDeleted", "true"))
        .and(query_param("maxResults", "250"))
        .and(query_param_is_missing("orderBy"))
        .and(query_param_is_missing("syncToken"))
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
    let got = sync_me(&source, None).await.unwrap();
    let ts = |s: &str| s.parse::<Timestamp>().unwrap();
    assert_eq!(got.calendar_id, "me@example.com");
    assert_eq!(got.next_token.as_deref(), Some("sync-1"));
    assert_eq!(
        got.changes,
        vec![
            CalendarChange::Upsert(FetchedEvent {
                calendar_id: "me@example.com".into(),
                source_id: "a".into(),
                instance_start: None,
                title: "Standup".into(),
                start: ts("2026-11-21T08:00:00Z"),
                end: ts("2026-11-21T08:15:00Z"),
            }),
            CalendarChange::Cancelled(EventKey {
                calendar_id: "me@example.com".into(),
                source_id: "gone".into(),
                instance_start: None,
            }),
            CalendarChange::Upsert(FetchedEvent {
                calendar_id: "me@example.com".into(),
                source_id: "weekly_20261121T140000Z".into(),
                instance_start: Some(ts("2026-11-21T14:00:00Z")),
                title: "(no title)".into(),
                start: ts("2026-11-21T15:00:00Z"),
                end: ts("2026-11-21T15:30:00Z"),
            }),
        ]
    );
    // A second fetch reuses the refreshed access token (the token mock expects 1).
    sync_me(&source, None).await.unwrap();
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn an_http_failure_is_an_other_error_without_details() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Standup details"))
        .mount(&server)
        .await;
    let source = signed_in(&server);
    let err = sync_me(&source, None).await.unwrap_err();
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
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_sign_in_that_cannot_be_stored_lasts_for_the_session() {
    let server = MockServer::start().await;
    // A short-lived access token, so the next fetch must refresh.
    let mut first = token_json(Some("rt-1"), "at-1");
    first["expires_in"] = json!(30);
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=rt-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(None, "at-2")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/me@example.com/events"))
        .and(header("authorization", "Bearer at-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/users/me/calendarList"))
        .respond_with(ResponseTemplate::new(503).set_body_string("rt-1"))
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        Arc::new(BrokenSecrets),
        browser(seen, None),
    );
    assert_eq!(source.sign_in().await.unwrap(), SessionStore::SessionOnly);
    // The in-memory refresh token signs the next fetch.
    assert_eq!(sync_me(&source, None).await.unwrap().changes, vec![]);
    let err = source.list_calendars().await.unwrap_err();
    assert!(!err.message.contains("rt-1"), "{err}");

    // Signing out forgets it without touching the (broken) store.
    source.sign_out().await.unwrap();
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert!(!err.message.contains("rt-1"), "{err}");
}

#[tokio::test]
async fn a_stored_sign_in_reports_stored() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .mount(&server)
        .await;
    let secrets = Arc::new(MemorySecrets::default());
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(config(&server), secrets.clone(), browser(seen, None));
    assert_eq!(source.sign_in().await.unwrap(), SessionStore::Stored);
    assert_eq!(secrets.refresh().as_deref(), Some("rt-1"));
}

#[tokio::test]
async fn the_users_own_client_replaces_the_built_in_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_id=my-own-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .expect(1)
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        Arc::new(MemorySecrets::default()),
        browser(seen.clone(), None),
    );
    source.set_client(Some(OAuthClient {
        id: "my-own-id".into(),
        secret: None,
    }));
    assert!(source.has_client());
    source.sign_in().await.unwrap();

    let auth_url = seen.lock().unwrap().clone().unwrap();
    let params: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "my-own-id");
    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("client_id=my-own-id"), "{body}");
    assert!(!body.contains("client-id"), "{body}");
    // No secret configured: none is sent.
    assert!(!body.contains("client_secret"), "{body}");

    // Clearing it goes back to the built-in client.
    source.set_client(None);
    assert!(source.has_client());
}

#[tokio::test]
async fn with_no_client_at_all_has_client_is_false_and_sign_in_fails() {
    let server = MockServer::start().await;
    let config = GcalConfig::with_base(&server.uri(), None, Duration::from_secs(1));
    let source = GcalSource::new(config, Arc::new(MemorySecrets::default()), no_browser());
    assert!(!source.has_client());
    let err = source.sign_in().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NoClient);
    source.set_client(Some(OAuthClient {
        id: "mine".into(),
        secret: None,
    }));
    assert!(source.has_client());
}

#[tokio::test]
async fn sign_out_deletes_the_stored_token_and_the_next_fetch_is_not_signed_in() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    let secrets = MemorySecrets::with_refresh("rt");
    let source = GcalSource::new(config(&server), secrets.clone(), no_browser());
    sync_me(&source, None).await.unwrap();
    let before = server.received_requests().await.unwrap().len();

    source.sign_out().await.unwrap();
    assert_eq!(secrets.refresh(), None);
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    // The cached access token is gone too: no request was made.
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

#[tokio::test]
async fn sign_out_reports_a_delete_failure() {
    let server = MockServer::start().await;
    let source = GcalSource::new(config(&server), Arc::new(BrokenSecrets), no_browser());
    let err = source.sign_out().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::Other);
    assert_eq!(err.message, LOCKED);
}

#[tokio::test]
async fn a_refresh_answered_invalid_grant_deletes_the_token_and_expires_the_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Token has been expired or revoked. rt-9",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let secrets = MemorySecrets::with_refresh("rt-9");
    let source = GcalSource::new(config(&server), secrets.clone(), no_browser());
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::SessionExpired);
    assert!(!err.message.contains("rt-9"), "{err}");
    assert_eq!(secrets.refresh(), None);
    // The session is gone: the next fetch makes no request.
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
}

#[tokio::test]
async fn an_expired_session_ignores_a_failing_delete() {
    let server = MockServer::start().await;
    let mut first = token_json(Some("rt-1"), "at-1");
    first["expires_in"] = json!(30);
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(
        config(&server),
        Arc::new(BrokenSecrets),
        browser(seen, None),
    );
    source.sign_in().await.unwrap();
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::SessionExpired);
}

#[tokio::test]
async fn a_sign_in_completing_after_a_sign_out_stores_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .expect(0)
        .mount(&server)
        .await;
    let secrets = Arc::new(MemorySecrets::default());
    let seen = Arc::new(Mutex::new(None));
    let (open, gate) = std::sync::mpsc::channel();
    let source = GcalSource::new(
        config(&server),
        secrets.clone(),
        gated_browser(seen.clone(), None, Some(gate)),
    );
    let signing_in = tokio::spawn({
        let source = source.clone();
        async move { source.sign_in().await }
    });
    while seen.lock().unwrap().is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    source.sign_out().await.unwrap();
    open.send(()).unwrap();
    assert!(signing_in.await.unwrap().is_err());
    assert_eq!(secrets.refresh(), None);
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
}

/// A secret store holding a token it cannot delete, counting its reads.
struct UndeletableSecrets {
    inner: MemorySecrets,
    gets: std::sync::atomic::AtomicUsize,
}

impl SecretStore for UndeletableSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(key)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        self.inner.set(key, value)
    }

    fn delete(&self, _key: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::new("the keyring is locked"))
    }
}

#[tokio::test]
async fn after_a_sign_out_whose_delete_failed_the_stored_token_is_not_read_again() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    let secrets = Arc::new(UndeletableSecrets {
        inner: MemorySecrets::default(),
        gets: Default::default(),
    });
    secrets.inner.set(REFRESH_TOKEN_KEY, "rt").unwrap();
    let source = GcalSource::new(config(&server), secrets.clone(), no_browser());
    sync_me(&source, None).await.unwrap();
    let gets = secrets.gets.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(gets, 1);
    let requests = server.received_requests().await.unwrap().len();

    assert!(source.sign_out().await.is_err());
    let err = sync_me(&source, None).await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    assert_eq!(secrets.gets.load(std::sync::atomic::Ordering::SeqCst), gets);
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

/// A secret store whose `set` blocks until `gate` gets a message.
struct SlowSetSecrets {
    inner: MemorySecrets,
    started: AtomicBool,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SecretStore for SlowSetSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        self.inner.get(key)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        self.started.store(true, Ordering::SeqCst);
        let _ = self.gate.lock().unwrap().recv();
        self.inner.set(key, value)
    }

    fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_sign_out_after_aborting_a_sign_in_mid_store_still_deletes_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(Some("rt-1"), "at-1")))
        .mount(&server)
        .await;
    let (release, gate) = std::sync::mpsc::channel();
    let secrets = Arc::new(SlowSetSecrets {
        inner: MemorySecrets::default(),
        started: AtomicBool::new(false),
        gate: Mutex::new(gate),
    });
    let seen = Arc::new(Mutex::new(None));
    let source = GcalSource::new(config(&server), secrets.clone(), browser(seen, None));
    let signing_in = tokio::spawn({
        let source = source.clone();
        async move { source.sign_in().await }
    });
    while !secrets.started.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The engine aborts the sign-in while the keyring write is still running.
    signing_in.abort();
    let signing_out = tokio::spawn(source.sign_out());
    tokio::time::sleep(Duration::from_millis(20)).await;
    release.send(()).unwrap();
    signing_out.await.unwrap().unwrap();
    assert_eq!(
        secrets.inner.refresh(),
        None,
        "the delete ran after the store"
    );
}

/// A token mock answering any refresh with access token `at`.
async fn mount_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(None, "at")))
        .mount(server)
        .await;
}

fn signed_in(server: &MockServer) -> GcalSource {
    GcalSource::new(
        config(server),
        MemorySecrets::with_refresh("rt"),
        no_browser(),
    )
}

#[tokio::test]
async fn an_incremental_sync_sends_only_the_token_and_parses_cancellations() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // The calendar is named by the caller: no calendar lookup.
    Mock::given(method("GET"))
        .and(path("/calendar/v3/users/me/calendarList/primary"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "x" })))
        .expect(0)
        .mount(&server)
        .await;
    let events = "/calendar/v3/calendars/me@example.com/events";
    let page1 = json!({
        "items": [
            timed("moved", "Moved away", "2026-11-23T09:00:00Z", "2026-11-23T10:00:00Z"),
            { "id": "single", "status": "cancelled" },
            {
                "id": "weekly_20261121T140000Z", "status": "cancelled",
                "recurringEventId": "weekly",
                "originalStartTime": { "dateTime": "2026-11-21T14:00:00Z" },
            },
        ],
        "nextPageToken": "p2",
    });
    let page2 = json!({
        "items": [
            timed("today", "Retitled", "2026-11-21T10:00:00Z", "2026-11-21T10:30:00Z"),
        ],
        "nextSyncToken": "sync-2",
    });
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("syncToken", "sync-1"))
        .and(query_param("singleEvents", "true"))
        .and(query_param("showDeleted", "true"))
        .and(query_param("maxResults", "250"))
        .and(query_param_is_missing("timeMin"))
        .and(query_param_is_missing("timeMax"))
        .and(query_param_is_missing("orderBy"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page1))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("syncToken", "sync-1"))
        .and(query_param("pageToken", "p2"))
        .and(query_param_is_missing("timeMin"))
        .and(query_param_is_missing("timeMax"))
        .and(query_param_is_missing("orderBy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page2))
        .expect(1)
        .mount(&server)
        .await;

    let source = signed_in(&server);
    let got = sync_me(&source, Some("sync-1")).await.unwrap();
    let ts = |s: &str| s.parse::<Timestamp>().unwrap();
    assert_eq!(got.calendar_id, "me@example.com");
    assert_eq!(got.next_token.as_deref(), Some("sync-2"));
    assert_eq!(
        got.changes,
        vec![
            CalendarChange::Cancelled(EventKey {
                calendar_id: "me@example.com".into(),
                source_id: "single".into(),
                instance_start: None,
            }),
            CalendarChange::Cancelled(EventKey {
                calendar_id: "me@example.com".into(),
                source_id: "weekly_20261121T140000Z".into(),
                instance_start: Some(ts("2026-11-21T14:00:00Z")),
            }),
            CalendarChange::Upsert(FetchedEvent {
                calendar_id: "me@example.com".into(),
                source_id: "today".into(),
                instance_start: None,
                title: "Retitled".into(),
                start: ts("2026-11-21T10:00:00Z"),
                end: ts("2026-11-21T10:30:00Z"),
            }),
        ]
    );
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn a_last_page_without_a_sync_token_yields_none() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/me@example.com/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                timed("a", "Standup", "2026-11-21T09:00:00Z", "2026-11-21T09:15:00Z"),
            ],
        })))
        .mount(&server)
        .await;
    let got = sync_me(&signed_in(&server), None).await.unwrap();
    assert_eq!(got.changes.len(), 1);
    assert_eq!(got.next_token, None);
}

#[tokio::test]
async fn limits_and_an_expired_sync_token_have_their_own_kinds() {
    for (code, kind) in [
        (403, CalendarErrorKind::RateLimited),
        (429, CalendarErrorKind::RateLimited),
        (410, CalendarErrorKind::Gone),
    ] {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(code).set_body_string("Standup sync-1"))
            .mount(&server)
            .await;
        let err = sync_me(&signed_in(&server), Some("sync-1"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, kind, "{code}");
        assert!(err.message.contains(&code.to_string()), "{err}");
        assert!(!err.message.contains("sync-1"), "{err}");
        assert!(!err.message.contains("Standup"), "{err}");
        assert_get_only(&server.received_requests().await.unwrap());
    }
}

#[tokio::test]
async fn a_full_sync_needs_no_calendar_lookup() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/calendar/v3/calendars/me@example.com/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    sync_me(&signed_in(&server), None).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().contains("calendarList")),
        "no calendarList request"
    );
}

#[tokio::test]
async fn a_non_primary_calendar_id_is_percent_encoded_and_names_the_batch() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let id = "en.usa#holiday@group.v.calendar.google.com";
    Mock::given(method("GET"))
        .and(path(
            "/calendar/v3/calendars/en.usa%23holiday@group.v.calendar.google.com/events",
        ))
        .and(query_param("timeMin", "2026-11-21T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                timed("h", "Holiday call", "2026-11-21T09:00:00Z", "2026-11-21T09:30:00Z"),
            ],
            "nextSyncToken": "s-h",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (from, to) = window();
    let got = signed_in(&server)
        .sync_calendar(id.into(), None, from, to)
        .await
        .unwrap();
    assert_eq!(got.calendar_id, id);
    assert_eq!(got.next_token.as_deref(), Some("s-h"));
    match &got.changes[..] {
        [CalendarChange::Upsert(e)] => assert_eq!(e.calendar_id, id),
        other => panic!("{other:?}"),
    }
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn list_calendars_pages_prefers_the_override_and_reads_primary() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let list = "/calendar/v3/users/me/calendarList";
    Mock::given(method("GET"))
        .and(path(list))
        .and(query_param("minAccessRole", "reader"))
        .and(query_param("maxResults", "250"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                { "id": "me@example.com", "summary": "me@example.com", "primary": true },
                { "id": "team@group", "summary": "Team", "summaryOverride": "My team" },
            ],
            "nextPageToken": "p2",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(list))
        .and(query_param("minAccessRole", "reader"))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                { "id": "x#holiday@group", "summary": "Holidays", "primary": false },
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let got = signed_in(&server).list_calendars().await.unwrap();
    let flat: Vec<(&str, &str, bool)> = got
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.primary))
        .collect();
    assert_eq!(
        flat,
        vec![
            ("me@example.com", "me@example.com", true),
            ("team@group", "My team", false),
            ("x#holiday@group", "Holidays", false),
        ]
    );
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn list_calendars_failures_keep_their_kinds() {
    let server = MockServer::start().await;
    let err = GcalSource::new(
        config(&server),
        Arc::new(MemorySecrets::default()),
        no_browser(),
    )
    .list_calendars()
    .await
    .unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::NotSignedIn);
    mount_token(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let err = signed_in(&server).list_calendars().await.unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::RateLimited);
}

#[tokio::test]
async fn list_events_pages_drops_cancelled_and_skipped_events_and_sends_no_sync_options() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let events = "/calendar/v3/calendars/o@example.com/events";
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                timed("c", "Late", "2026-11-21T15:00:00Z", "2026-11-21T15:30:00Z"),
            ],
            "nextSyncToken": "s-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(events))
        .and(query_param("timeMin", "2026-11-21T00:00:00Z"))
        .and(query_param("timeMax", "2026-11-22T00:00:00Z"))
        .and(query_param("singleEvents", "true"))
        .and(query_param_is_missing("showDeleted"))
        .and(query_param_is_missing("syncToken"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                timed("a", "Standup", "2026-11-21T09:00:00Z", "2026-11-21T09:15:00Z"),
                { "id": "x", "status": "cancelled" },
                { "id": "allday", "status": "confirmed", "summary": "Off",
                  "start": { "date": "2026-11-21" }, "end": { "date": "2026-11-22" } },
                timed("tomorrow", "Later", "2026-11-22T09:00:00Z", "2026-11-22T09:15:00Z"),
            ],
            "nextPageToken": "p2",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (from, to) = window();
    let got = signed_in(&server)
        .list_events("o@example.com".into(), from, to)
        .await
        .unwrap();
    let ids: Vec<(&str, &str, &str)> = got
        .iter()
        .map(|e| {
            (
                e.calendar_id.as_str(),
                e.source_id.as_str(),
                e.title.as_str(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            ("o@example.com", "a", "Standup"),
            ("o@example.com", "c", "Late")
        ]
    );
    let requests = server.received_requests().await.unwrap();
    assert_get_only(&requests);
    assert!(
        requests
            .iter()
            .all(|r| !r.url.query().unwrap_or("").contains("showDeleted")),
    );
}

async fn get_one(source: &GcalSource, calendar: &str, event: &str) -> Option<CalendarChange> {
    let (from, to) = window();
    source
        .get_event(calendar.into(), event.into(), from, to)
        .await
        .unwrap()
}

#[tokio::test]
async fn get_event_reads_a_timed_event_with_its_original_start() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let mut body = timed(
        "r1_20261121",
        "Weekly",
        "2026-11-21T10:00:00Z",
        "2026-11-21T10:30:00Z",
    );
    body["originalStartTime"] = json!({ "dateTime": "2026-11-21T09:00:00Z" });
    Mock::given(method("GET"))
        .and(path(
            "/calendar/v3/calendars/o@example.com/events/r1_20261121",
        ))
        .and(header("authorization", "Bearer at"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let got = get_one(&signed_in(&server), "o@example.com", "r1_20261121").await;
    match got {
        Some(CalendarChange::Upsert(e)) => {
            assert_eq!(e.calendar_id, "o@example.com");
            assert_eq!(e.source_id, "r1_20261121");
            assert_eq!(e.title, "Weekly");
            assert_eq!(e.start, "2026-11-21T10:00:00Z".parse().unwrap());
            assert_eq!(
                e.instance_start,
                Some("2026-11-21T09:00:00Z".parse().unwrap())
            );
        }
        other => panic!("{other:?}"),
    }
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn get_event_maps_cancelled_skipped_and_missing_events() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let at = |id: &str| format!("/calendar/v3/calendars/o@example.com/events/{id}");
    Mock::given(method("GET"))
        .and(path(at("gone")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": "gone", "status": "cancelled" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(at("tomorrow")))
        .respond_with(ResponseTemplate::new(200).set_body_json(timed(
            "tomorrow",
            "Later",
            "2026-11-22T09:00:00Z",
            "2026-11-22T09:15:00Z",
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(at("missing")))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;
    let source = signed_in(&server);
    assert_eq!(
        get_one(&source, "o@example.com", "gone").await,
        Some(CalendarChange::Cancelled(EventKey {
            calendar_id: "o@example.com".into(),
            source_id: "gone".into(),
            instance_start: None,
        }))
    );
    assert_eq!(get_one(&source, "o@example.com", "tomorrow").await, None);
    assert_eq!(get_one(&source, "o@example.com", "missing").await, None);
    assert_get_only(&server.received_requests().await.unwrap());
}

#[tokio::test]
async fn get_event_percent_encodes_both_ids_and_keeps_error_kinds() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path(
            "/calendar/v3/calendars/en.usa%23holiday@group.v.calendar.google.com/events/a%2Fb%3Fc",
        ))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let (from, to) = window();
    let err = signed_in(&server)
        .get_event(
            "en.usa#holiday@group.v.calendar.google.com".into(),
            "a/b?c".into(),
            from,
            to,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, CalendarErrorKind::RateLimited);
    assert!(!err.message.contains("holiday"), "{err}");
    assert_get_only(&server.received_requests().await.unwrap());
}
