//! The Google Calendar source behind the [`CalendarSource`] port (CAP-2, AD-9,
//! AD-13).
//!
//! Sign-in is the desktop OAuth flow: a loopback redirect on `127.0.0.1` at an
//! ephemeral port, PKCE S256, and a random `state` checked on the callback. The
//! browser is opened through a callback the shell injects, so this crate never
//! depends on Tauri (AD-1). The refresh token goes to the [`SecretStore`] and is
//! kept in memory for the session; it is never logged or put in an error message.
//! Access tokens are cached in memory until shortly before they expire.
//!
//! Only GET requests go to the Calendar API, and POST only to the token endpoint.
//! OAuth is driven by `oauth2` over this crate's `reqwest` client, so no second
//! HTTP stack is compiled.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nguda_core::{
    CalendarChange, CalendarError, CalendarErrorKind, CalendarFuture, CalendarInfo, CalendarSource,
    EventKey, FetchedEvent, SecretStore, SyncBatch, Timestamp,
};
use oauth2::basic::{BasicClient, BasicRequestTokenError};
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, HttpRequest, HttpResponse, PkceCodeChallenge, RedirectUrl, RefreshToken, Scope,
    TokenResponse, TokenUrl,
};
use reqwest::Url;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The secret store key of the Google refresh token.
pub const REFRESH_TOKEN_KEY: &str = "google-refresh-token";

/// The read-only scopes nguda asks for (AD-9), exactly these two.
pub const SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/calendar.events.readonly",
    "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
];

/// Opens a URL in the system browser. Injected by the shell (AD-1).
pub type OpenUrl = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Where and as whom the source talks to Google.
#[derive(Clone)]
pub struct GcalConfig {
    /// The OAuth client id and secret, or `None` when the build has none.
    pub client: Option<(String, String)>,
    pub auth_url: String,
    pub token_url: String,
    /// The Calendar API base URL, ending in `/`.
    pub api_base: String,
    /// How long sign-in waits for the browser to come back.
    pub redirect_timeout: Duration,
}

impl fmt::Debug for GcalConfig {
    /// Never prints the client secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcalConfig")
            .field("client", &self.client.as_ref().map(|(id, _)| id))
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("api_base", &self.api_base)
            .field("redirect_timeout", &self.redirect_timeout)
            .finish()
    }
}

impl GcalConfig {
    /// Google's endpoints, with the client built in from `NGUDA_GOOGLE_CLIENT_ID`
    /// and `NGUDA_GOOGLE_CLIENT_SECRET` at compile time (`None` unless both are
    /// set and non-empty).
    pub fn google() -> Self {
        let client = match (
            option_env!("NGUDA_GOOGLE_CLIENT_ID"),
            option_env!("NGUDA_GOOGLE_CLIENT_SECRET"),
        ) {
            (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                Some((id.to_owned(), secret.to_owned()))
            }
            _ => None,
        };
        Self {
            client,
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            api_base: "https://www.googleapis.com/calendar/v3/".into(),
            redirect_timeout: Duration::from_secs(5 * 60),
        }
    }

    /// The same as [`GcalConfig::google`] with every endpoint under `base` and the
    /// given client and timeout: for tests against a mock server.
    pub fn with_base(
        base: &str,
        client: Option<(String, String)>,
        redirect_timeout: Duration,
    ) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            client,
            auth_url: format!("{base}/o/oauth2/v2/auth"),
            token_url: format!("{base}/token"),
            api_base: format!("{base}/calendar/v3/"),
            redirect_timeout,
        }
    }
}

/// The Google Calendar implementation of [`CalendarSource`]. Cheap to share.
#[derive(Clone)]
pub struct GcalSource {
    inner: Arc<Inner>,
}

struct Inner {
    config: GcalConfig,
    secrets: Arc<dyn SecretStore>,
    open_url: OpenUrl,
    http: reqwest::Client,
    /// The refresh token, once read from or written to the secret store.
    refresh: Mutex<Option<String>>,
    /// The current access token and when to stop using it.
    access: Mutex<Option<(String, Instant)>>,
}

type Client = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

impl GcalSource {
    pub fn new(config: GcalConfig, secrets: Arc<dyn SecretStore>, open_url: OpenUrl) -> Self {
        let http = reqwest::Client::builder()
            // OAuth endpoints must never be followed through a redirect.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                config,
                secrets,
                open_url,
                http,
                refresh: Mutex::new(None),
                access: Mutex::new(None),
            }),
        }
    }
}

impl CalendarSource for GcalSource {
    fn sign_in(&self) -> CalendarFuture<()> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.sign_in().await })
    }

    fn list_calendars(&self) -> CalendarFuture<Vec<CalendarInfo>> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.list_calendars().await })
    }

    fn sync_calendar(
        &self,
        calendar_id: String,
        token: Option<String>,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<SyncBatch> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner
                .sync_calendar(calendar_id, token, window_start, window_end)
                .await
        })
    }
}

fn auth(message: impl Into<String>) -> CalendarError {
    CalendarError::new(CalendarErrorKind::Auth, message)
}

fn other(message: impl Into<String>) -> CalendarError {
    CalendarError::new(CalendarErrorKind::Other, message)
}

fn no_client() -> CalendarError {
    CalendarError::new(
        CalendarErrorKind::NoClient,
        "no Google OAuth client built in",
    )
}

/// A transport failure, without the URL (it may name the account's calendar).
fn transport(e: reqwest::Error) -> CalendarError {
    let e = e.without_url();
    if e.is_timeout() {
        other("Google did not answer in time")
    } else {
        other("could not reach Google")
    }
}

impl Inner {
    fn client(&self, redirect: Option<&str>) -> Result<Client, CalendarError> {
        let (id, secret) = self.config.client.as_ref().ok_or_else(no_client)?;
        let bad = |what: &str| other(format!("invalid {what} URL"));
        let mut client = BasicClient::new(ClientId::new(id.clone()))
            .set_client_secret(ClientSecret::new(secret.clone()))
            .set_auth_type(AuthType::RequestBody)
            .set_auth_uri(AuthUrl::new(self.config.auth_url.clone()).map_err(|_| bad("auth"))?)
            .set_token_uri(TokenUrl::new(self.config.token_url.clone()).map_err(|_| bad("token"))?);
        if let Some(redirect) = redirect {
            client = client.set_redirect_uri(
                RedirectUrl::new(redirect.to_owned()).map_err(|_| bad("redirect"))?,
            );
        }
        Ok(client)
    }

    /// The oauth2 HTTP hook over this source's `reqwest` client.
    fn oauth_http(
        &self,
    ) -> impl Fn(HttpRequest) -> Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send>>
    {
        let http = self.http.clone();
        move |request: HttpRequest| {
            let http = http.clone();
            Box::pin(async move {
                let request = reqwest::Request::try_from(request)
                    .map_err(|_| HttpError("invalid token request"))?;
                let response = http
                    .execute(request)
                    .await
                    .map_err(|_| HttpError("could not reach the token endpoint"))?;
                let mut builder = oauth2::http::Response::builder().status(response.status());
                for (name, value) in response.headers() {
                    builder = builder.header(name, value);
                }
                let body = response
                    .bytes()
                    .await
                    .map_err(|_| HttpError("the token response was cut off"))?;
                builder
                    .body(body.to_vec())
                    .map_err(|_| HttpError("invalid token response"))
            })
        }
    }

    async fn sign_in(&self) -> Result<(), CalendarError> {
        // Fail before opening anything when there is no client.
        self.config.client.as_ref().ok_or_else(no_client)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| other("could not open the sign-in redirect port"))?;
        let port = listener
            .local_addr()
            .map_err(|_| other("could not open the sign-in redirect port"))?
            .port();
        let redirect = format!("http://127.0.0.1:{port}");
        let client = self.client(Some(&redirect))?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client.authorize_url(CsrfToken::new_random);
        for scope in SCOPES {
            request = request.add_scope(Scope::new(scope.to_owned()));
        }
        let (url, state) = request
            .set_pkce_challenge(challenge)
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent")
            .url();
        (self.open_url)(url.as_str()).map_err(|_| other("could not open the browser"))?;
        let code = tokio::time::timeout(
            self.config.redirect_timeout,
            wait_for_code(&listener, state.secret()),
        )
        .await
        .map_err(|_| auth("sign-in timed out"))??;
        drop(listener);
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(&self.oauth_http())
            .await
            .map_err(token_error)?;
        let refresh = token
            .refresh_token()
            .ok_or_else(|| auth("Google returned no refresh token"))?
            .secret()
            .clone();
        let secrets = self.secrets.clone();
        let stored = refresh.clone();
        tokio::task::spawn_blocking(move || secrets.set(REFRESH_TOKEN_KEY, &stored))
            .await
            .map_err(|_| other("the keyring task failed"))?
            .map_err(|e| other(format!("could not store the sign-in: {}", e.message)))?;
        // Only a stored sign-in counts: cache the access token after the store.
        self.cache_access(token.access_token().secret().clone(), token.expires_in());
        *self.refresh.lock().unwrap() = Some(refresh);
        Ok(())
    }

    fn cache_access(&self, token: String, expires_in: Option<Duration>) {
        // Stop a minute early, so a token never expires mid-request.
        let life = expires_in
            .unwrap_or(Duration::from_secs(3600))
            .saturating_sub(Duration::from_secs(60));
        *self.access.lock().unwrap() = Some((token, Instant::now() + life));
    }

    /// The refresh token: from memory, else from the secret store.
    async fn refresh_token(&self) -> Result<Option<String>, CalendarError> {
        if let Some(token) = self.refresh.lock().unwrap().clone() {
            return Ok(Some(token));
        }
        let secrets = self.secrets.clone();
        let token = tokio::task::spawn_blocking(move || secrets.get(REFRESH_TOKEN_KEY))
            .await
            .map_err(|_| other("the keyring task failed"))?
            // An unreadable keyring holds no usable session: report not signed in,
            // so the user is offered Sign in rather than shown a phantom account.
            .map_err(|e| {
                CalendarError::new(
                    CalendarErrorKind::NotSignedIn,
                    format!("could not read the sign-in: {}", e.message),
                )
            })?;
        if let Some(token) = &token {
            *self.refresh.lock().unwrap() = Some(token.clone());
        }
        Ok(token)
    }

    /// A valid access token: the cached one, else a fresh one from the refresh
    /// token. No refresh token means not signed in.
    async fn access_token(&self) -> Result<String, CalendarError> {
        if let Some((token, until)) = self.access.lock().unwrap().clone()
            && Instant::now() < until
        {
            return Ok(token);
        }
        let refresh = self.refresh_token().await?.ok_or_else(|| {
            CalendarError::new(CalendarErrorKind::NotSignedIn, "not signed in to Google")
        })?;
        let client = self.client(None)?;
        let refresh = RefreshToken::new(refresh);
        let token = client
            .exchange_refresh_token(&refresh)
            .request_async(&self.oauth_http())
            .await
            .map_err(token_error)?;
        let access = token.access_token().secret().clone();
        self.cache_access(access.clone(), token.expires_in());
        Ok(access)
    }

    /// A GET to the Calendar API, parsed as `T`.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        url: Url,
        token: &str,
    ) -> Result<T, CalendarError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            *self.access.lock().unwrap() = None;
            return Err(auth("Google rejected the access token"));
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(CalendarError::new(
                CalendarErrorKind::RateLimited,
                format!("Google is limiting requests (HTTP {})", status.as_u16()),
            ));
        }
        if status == reqwest::StatusCode::GONE {
            return Err(CalendarError::new(
                CalendarErrorKind::Gone,
                "the calendar sync token expired (HTTP 410)",
            ));
        }
        if !status.is_success() {
            return Err(other(format!("Calendar API HTTP {}", status.as_u16())));
        }
        response
            .json::<T>()
            .await
            .map_err(|_| other("the Calendar API response could not be read"))
    }

    fn api_url(&self, segments: &[&str]) -> Result<Url, CalendarError> {
        let mut url =
            Url::parse(&self.config.api_base).map_err(|_| other("invalid Calendar API URL"))?;
        url.path_segments_mut()
            .map_err(|()| other("invalid Calendar API URL"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    /// The account's readable calendars (see [`CalendarSource::list_calendars`]):
    /// `users/me/calendarList` with `minAccessRole=reader`, following every page.
    /// The name is the user's `summaryOverride` when set, else the `summary`.
    /// GET only.
    async fn list_calendars(&self) -> Result<Vec<CalendarInfo>, CalendarError> {
        let token = self.access_token().await?;
        let mut calendars = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = self.api_url(&["users", "me", "calendarList"])?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("minAccessRole", "reader")
                    .append_pair("maxResults", "250");
                if let Some(page) = &page_token {
                    query.append_pair("pageToken", page);
                }
            }
            let page: CalendarListPage = self.get(url, &token).await?;
            calendars.extend(page.items.into_iter().map(CalendarListEntry::info));
            match page.next_page_token {
                Some(next) if !next.is_empty() => page_token = Some(next),
                _ => break,
            }
        }
        Ok(calendars)
    }

    /// One sync of the calendar `calendar_id` (see
    /// [`CalendarSource::sync_calendar`]). A full fetch lists the window's events;
    /// an incremental one sends the sync token only. Both send `singleEvents` and
    /// `showDeleted`, never `orderBy`, follow every page, and read `nextSyncToken`
    /// from the last one. GET only.
    async fn sync_calendar(
        &self,
        calendar_id: String,
        sync_token: Option<String>,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> Result<SyncBatch, CalendarError> {
        let token = self.access_token().await?;
        let mut changes = Vec::new();
        let mut page_token: Option<String> = None;
        let next_token = loop {
            // Path segments are percent-encoded, so any calendar id is safe.
            let mut url = self.api_url(&["calendars", &calendar_id, "events"])?;
            {
                let mut query = url.query_pairs_mut();
                match &sync_token {
                    Some(sync) => {
                        query.append_pair("syncToken", sync);
                    }
                    None => {
                        query
                            .append_pair("timeMin", &window_start.to_string())
                            .append_pair("timeMax", &window_end.to_string());
                    }
                }
                query
                    .append_pair("singleEvents", "true")
                    .append_pair("showDeleted", "true")
                    .append_pair("maxResults", "250");
                if let Some(page) = &page_token {
                    query.append_pair("pageToken", page);
                }
            }
            let page: EventsPage = self.get(url, &token).await?;
            changes.extend(
                page.items
                    .into_iter()
                    .filter_map(|e| e.change(&calendar_id, window_start, window_end)),
            );
            match page.next_page_token {
                Some(next) if !next.is_empty() => page_token = Some(next),
                _ => break page.next_sync_token.filter(|t| !t.is_empty()),
            }
        };
        Ok(SyncBatch {
            calendar_id,
            changes,
            next_token,
        })
    }
}

/// A token endpoint failure as a message with the OAuth error code at most, never
/// a token or the raw response body.
fn token_error(e: BasicRequestTokenError<HttpError>) -> CalendarError {
    match e {
        oauth2::RequestTokenError::ServerResponse(r) => {
            auth(format!("Google refused the sign-in ({})", r.error()))
        }
        oauth2::RequestTokenError::Request(e) => other(e.0),
        oauth2::RequestTokenError::Parse(..) => auth("the token response could not be read"),
        oauth2::RequestTokenError::Other(_) => auth("the token request failed"),
    }
}

/// An HTTP failure of the oauth2 hook, with a fixed message.
#[derive(Debug)]
pub struct HttpError(&'static str);

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for HttpError {}

/// The page the browser shows after the redirect.
const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>nguda</title>\
<p>Signed in to nguda. You can close this window.</p>";
const FAILED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>nguda</title>\
<p>Sign-in did not complete. You can close this window and try again in nguda.</p>";

/// How long one loopback connection may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Serves the loopback redirect until a request carries `code` or `error`,
/// answering the browser each time. Returns the code once `state` matches; a
/// wrong state or an `error` fails the sign-in. Other requests (such as a favicon)
/// get a 404 and are ignored.
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, CalendarError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| other("the sign-in redirect port failed"))?;
        // A connection that sends nothing (a browser preconnect) must not block
        // the real redirect behind it.
        let Ok(Some(target)) = tokio::time::timeout(READ_TIMEOUT, read_target(&mut stream)).await
        else {
            continue;
        };
        let Ok(url) = Url::parse("http://127.0.0.1").and_then(|base| base.join(&target)) else {
            respond(&mut stream, "400 Bad Request", FAILED_PAGE).await;
            continue;
        };
        let get = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        let (code, error, returned_state) = (get("code"), get("error"), get("state"));
        if code.is_none() && error.is_none() {
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        if returned_state.as_deref() != Some(state) {
            respond(&mut stream, "400 Bad Request", FAILED_PAGE).await;
            return Err(auth("sign-in state mismatch"));
        }
        if let Some(code) = code {
            respond(&mut stream, "200 OK", DONE_PAGE).await;
            return Ok(code);
        }
        respond(&mut stream, "200 OK", FAILED_PAGE).await;
        return Err(auth("sign-in was cancelled in the browser"));
    }
}

/// The request target of an HTTP request on `stream`, from its first line.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = std::str::from_utf8(&buf).ok()?;
    let mut parts = head.lines().next()?.split_whitespace();
    let (_method, target) = (parts.next()?, parts.next()?);
    Some(target.to_owned())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // The browser may have gone away; the outcome stands either way.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListPage {
    #[serde(default)]
    items: Vec<CalendarListEntry>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListEntry {
    id: String,
    summary: Option<String>,
    summary_override: Option<String>,
    #[serde(default)]
    primary: bool,
}

impl CalendarListEntry {
    /// The entry as a listed calendar, named by its override when set.
    fn info(self) -> CalendarInfo {
        let name = self
            .summary_override
            .filter(|n| !n.is_empty())
            .or(self.summary)
            .unwrap_or_else(|| self.id.clone());
        CalendarInfo {
            id: self.id,
            name,
            primary: self.primary,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsPage {
    #[serde(default)]
    items: Vec<Event>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    status: Option<String>,
    summary: Option<String>,
    start: Option<EventTime>,
    end: Option<EventTime>,
    #[serde(default)]
    attendees: Vec<Attendee>,
    original_start_time: Option<EventTime>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventTime {
    date_time: Option<String>,
}

impl EventTime {
    fn instant(&self) -> Option<Timestamp> {
        self.date_time.as_deref()?.parse().ok()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attendee {
    #[serde(rename = "self", default)]
    is_self: bool,
    response_status: Option<String>,
}

impl Event {
    /// This event as a change of `calendar_id`: a cancellation (keyed on its id and
    /// a recurring instance's original start, whatever its time) or a timed upsert
    /// (see [`Event::timed`]), or `None` when it is skipped.
    fn change(
        self,
        calendar_id: &str,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> Option<CalendarChange> {
        if self.status.as_deref() == Some("cancelled") {
            return Some(CalendarChange::Cancelled(EventKey {
                calendar_id: calendar_id.to_owned(),
                instance_start: self
                    .original_start_time
                    .as_ref()
                    .and_then(EventTime::instant),
                source_id: self.id,
            }));
        }
        self.timed(calendar_id, window_start, window_end)
            .map(CalendarChange::Upsert)
    }

    /// This live event as a timed [`FetchedEvent`] of `calendar_id`, or `None` when
    /// it is all-day, declined by the account, does not end after it starts, or
    /// starts outside `[window_start, window_end)` (an event crossing midnight
    /// belongs to the day it starts on, AD-6; one moved to another day is not
    /// today's).
    fn timed(
        self,
        calendar_id: &str,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> Option<FetchedEvent> {
        let declined = self
            .attendees
            .iter()
            .any(|a| a.is_self && a.response_status.as_deref() == Some("declined"));
        if declined {
            return None;
        }
        let start = self.start.as_ref()?.instant()?;
        let end = self.end.as_ref()?.instant()?;
        if end <= start || start < window_start || start >= window_end {
            return None;
        }
        Some(FetchedEvent {
            calendar_id: calendar_id.to_owned(),
            source_id: self.id,
            instance_start: self
                .original_start_time
                .as_ref()
                .and_then(EventTime::instant),
            title: self.summary.unwrap_or_else(|| "(no title)".to_owned()),
            start,
            end,
        })
    }
}

#[cfg(test)]
mod tests;
