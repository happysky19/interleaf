//! Owner-driven browser login. Fridica retains ownership of credential storage.
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{Message, WebSocket};

const ORIGIN: &str = "https://www.overleaf.com";
/// Normalize an explicitly configured Overleaf origin. Plain HTTP is loopback-only.
pub fn normalize_base_url(value: &str) -> Result<String> {
    let uri: ureq::http::Uri = value.parse().map_err(|_| anyhow!("invalid Overleaf base URL"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| anyhow!("Overleaf base URL needs a scheme"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow!("Overleaf base URL needs a host"))?;
    let host = uri.host().ok_or_else(|| anyhow!("Overleaf base URL needs a host"))?;
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
    ensure!(
        (scheme == "https" || (scheme == "http" && loopback))
            && !authority.as_str().contains('@')
            && uri.query().is_none()
            && matches!(uri.path(), "" | "/")
            && value.len() <= 512,
        "Overleaf base URL must be an HTTPS origin (HTTP is allowed only on loopback)"
    );
    Ok(format!("{scheme}://{}", authority.as_str().to_ascii_lowercase()))
}

const BODY_LIMIT: u64 = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub user_id: String,
    pub email: String,
}

/// Contains a credential. Deliberately has no Debug or serialization implementation.
pub struct CapturedSession {
    pub cookie: String,
    pub identity: Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    Expired,
    Rejected,
    Unavailable,
    InvalidResponse,
}
impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Expired => "Overleaf session has expired",
            Self::Rejected => "Overleaf session or project was rejected",
            Self::Unavailable => "Overleaf session validation is unavailable",
            Self::InvalidResponse => "Overleaf returned invalid session metadata",
        })
    }
}
impl std::error::Error for ValidationError {}

/// Check Fridica's saved session against the fixed Overleaf origin, without redirects or proxies.
/// Only an explicit login redirect or HTTP 401 means that a new login is needed.
pub fn validate_cookie(cookie: &str, project: Option<&str>) -> std::result::Result<Identity, ValidationError> {
    validate(cookie, project, REQUEST_TIMEOUT)
}

/// Validate only against the owner-configured origin; never follow login redirects.
pub fn validate_cookie_at(
    base_url: &str,
    cookie: &str,
    project: Option<&str>,
) -> std::result::Result<Identity, ValidationError> {
    validate_on(base_url, cookie, project, REQUEST_TIMEOUT)
}

fn validate(cookie: &str, project: Option<&str>, timeout: Duration) -> std::result::Result<Identity, ValidationError> {
    validate_on(ORIGIN, cookie, project, timeout)
}

fn validate_on(
    base_url: &str,
    cookie: &str,
    project: Option<&str>,
    timeout: Duration,
) -> std::result::Result<Identity, ValidationError> {
    let origin = normalize_base_url(base_url).map_err(|_| ValidationError::Rejected)?;
    check_cookie(cookie)?;
    if project.is_some_and(|value| !id(value)) {
        return Err(ValidationError::Rejected);
    }
    let url = project.map_or_else(
        || format!("{origin}/project"),
        |value| format!("{origin}/project/{value}"),
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into();
    let mut cookie_header = ureq::http::HeaderValue::from_str(cookie).map_err(|_| ValidationError::Rejected)?;
    cookie_header.set_sensitive(true);
    let mut response = agent
        .get(&url)
        .header("Cookie", cookie_header)
        .header("Accept", "text/html")
        .call()
        .map_err(|_| ValidationError::Unavailable)?;
    status_on(
        &origin,
        response.status().as_u16(),
        response.headers().get("location").and_then(|v| v.to_str().ok()),
    )?;
    let mut raw = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(BODY_LIMIT + 1)
        .read_to_end(&mut raw)
        .map_err(|_| ValidationError::Unavailable)?;
    if raw.len() as u64 > BODY_LIMIT {
        return Err(ValidationError::InvalidResponse);
    }
    let page = String::from_utf8(raw).map_err(|_| ValidationError::InvalidResponse)?;
    parse_identity(&page, project)
}

#[cfg(test)]
fn status(code: u16, location: Option<&str>) -> std::result::Result<(), ValidationError> {
    status_on(ORIGIN, code, location)
}
fn status_on(origin: &str, code: u16, location: Option<&str>) -> std::result::Result<(), ValidationError> {
    match code {
        401 => Err(ValidationError::Expired),
        300..=399 => {
            let login = location
                .and_then(|value| value.parse::<ureq::http::Uri>().ok())
                .is_some_and(|uri| {
                    let origin_ok = match (uri.scheme_str(), uri.authority()) {
                        (None, None) => uri.path().starts_with('/') && !uri.path().starts_with("//"),
                        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}") == origin,
                        _ => false,
                    };
                    origin_ok && matches!(uri.path(), "/login" | "/login/")
                });
            Err(if login {
                ValidationError::Expired
            } else {
                ValidationError::Rejected
            })
        }
        200..=299 => Ok(()),
        500..=599 => Err(ValidationError::Unavailable),
        _ => Err(ValidationError::Rejected),
    }
}

fn id(value: &str) -> bool {
    value.len() == 24 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn email(value: &str) -> bool {
    value.len() <= 254
        && value
            .split_once('@')
            .is_some_and(|(name, host)| !name.is_empty() && !host.is_empty() && !host.contains('@'))
        && value.bytes().all(|b| (33..127).contains(&b))
}
fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.bytes().all(|b| (33..127).contains(&b) && !b";\"\\,".contains(&b))
}
fn check_cookie(cookie: &str) -> std::result::Result<(), ValidationError> {
    let mut names = BTreeSet::new();
    for pair in cookie.split("; ") {
        let (name, value) = pair.split_once('=').ok_or(ValidationError::Rejected)?;
        if !matches!(name, "overleaf_session2" | "GCLB") || !names.insert(name) || !token(value) {
            return Err(ValidationError::Rejected);
        }
    }
    if !names.contains("overleaf_session2") {
        return Err(ValidationError::Rejected);
    }
    Ok(())
}

fn meta(page: &str, name: &str) -> std::result::Result<String, ValidationError> {
    let mut found = None;
    for tag in crate::text::re!(r"(?is)<meta\s+[^>]*>").find_iter(page) {
        let mut attrs = BTreeMap::new();
        let mut duplicate = false;
        for captures in crate::text::re!(r#"([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).captures_iter(tag.as_str()) {
            let key = captures[1].to_ascii_lowercase();
            let value =
                html_escape::decode_html_entities(captures.get(2).or(captures.get(3)).unwrap().as_str()).into_owned();
            duplicate |= attrs.insert(key, value).is_some();
        }
        if attrs.get("name").map(String::as_str) == Some(name) {
            if duplicate || found.is_some() {
                return Err(ValidationError::InvalidResponse);
            }
            found = Some(attrs.remove("content").ok_or(ValidationError::InvalidResponse)?);
        }
    }
    found.ok_or(ValidationError::InvalidResponse)
}
fn parse_identity(page: &str, project: Option<&str>) -> std::result::Result<Identity, ValidationError> {
    if let Some(project) = project
        && meta(page, "ol-project_id")? != project
    {
        return Err(ValidationError::Rejected);
    }
    let user: Value = serde_json::from_str(&meta(page, "ol-user")?).map_err(|_| ValidationError::InvalidResponse)?;
    let user_id = user
        .get("_id")
        .or_else(|| user.get("id"))
        .and_then(Value::as_str)
        .ok_or(ValidationError::InvalidResponse)?;
    let user_email = user
        .get("email")
        .and_then(Value::as_str)
        .ok_or(ValidationError::InvalidResponse)?;
    if !id(user_id)
        || !email(user_email)
        || user
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|other| other != user_id)
    {
        return Err(ValidationError::InvalidResponse);
    }
    Ok(Identity {
        user_id: user_id.into(),
        email: user_email.into(),
    })
}

/// Launch a fresh isolated browser for the owner to complete password/MFA themselves.
/// Captured credentials are returned in memory; no browser-store import or credential persistence.
#[cfg(unix)]
pub fn capture_session(
    expected_email: &str,
    project: Option<&str>,
    browser: Option<&Path>,
    timeout: Duration,
) -> Result<CapturedSession> {
    capture_session_at(ORIGIN, expected_email, project, browser, timeout)
}

/// Owner-driven login against an explicit cloud or self-hosted origin.
#[cfg(unix)]
pub fn capture_session_at(
    base_url: &str,
    expected_email: &str,
    project: Option<&str>,
    browser: Option<&Path>,
    timeout: Duration,
) -> Result<CapturedSession> {
    let origin = normalize_base_url(base_url)?;
    ensure!(email(expected_email), "expected Overleaf email is invalid");
    ensure!(project.is_none_or(id), "Overleaf project ID is invalid");
    ensure!(!timeout.is_zero(), "browser login timeout must be positive");
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow!("browser login timeout is invalid"))?;
    let executable = browser_path(browser)?;
    let target = project.map_or_else(
        || format!("{origin}/login?redir=%2Fproject"),
        |value| format!("{origin}/login?redir=%2Fproject%2F{value}"),
    );
    let mut browser = Browser::start(&executable, &target)?;
    let result = browser.capture(&origin, expected_email, project, deadline);
    let cleanup = browser.close();
    match result {
        Err(error) => Err(error),
        Ok(session) => {
            cleanup?;
            Ok(session)
        }
    }
}

#[cfg(not(unix))]
pub fn capture_session(_: &str, _: Option<&str>, _: Option<&Path>, _: Duration) -> Result<CapturedSession> {
    bail!("owner browser login is supported on macOS and Linux")
}

#[cfg(not(unix))]
pub fn capture_session_at(_: &str, _: &str, _: Option<&str>, _: Option<&Path>, _: Duration) -> Result<CapturedSession> {
    bail!("owner browser login is supported on macOS and Linux")
}

#[cfg(unix)]
fn browser_path(requested: Option<&Path>) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/opt/google/chrome/chrome",
    ];
    let paths: Vec<&Path> = match requested {
        Some(path) => vec![path],
        None => candidates.iter().map(Path::new).collect(),
    };
    for path in paths {
        if !path.is_absolute() {
            continue;
        }
        if let Ok(path) = path.canonicalize()
            && let Ok(metadata) = path.metadata()
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Ok(path);
        }
    }
    bail!("Chrome or Chromium executable was not found")
}

#[cfg(test)]
fn cookies(values: &Value) -> Result<Option<String>> {
    cookies_at(values, ORIGIN)
}
fn cookies_at(values: &Value, origin: &str) -> Result<Option<String>> {
    let uri: ureq::http::Uri = origin.parse()?;
    let host = uri.host().ok_or_else(|| anyhow!("browser cookie origin missing"))?;
    let secure = uri.scheme_str() == Some("https");
    let values = values
        .as_array()
        .ok_or_else(|| anyhow!("browser cookie response is invalid"))?;
    ensure!(values.len() <= 4096, "browser cookie response exceeds limit");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let mut selected = BTreeMap::new();
    for value in values {
        let Some(name @ ("overleaf_session2" | "GCLB")) = value["name"].as_str() else {
            continue;
        };
        let domain = value["domain"].as_str().unwrap_or("");
        let matches = if let Some(domain) = domain.strip_prefix('.') {
            host == domain || host.ends_with(&format!(".{domain}"))
        } else {
            host == domain
        };
        if !matches || value["path"] != "/" || (secure && value["secure"] != true) {
            continue;
        }
        let live = value["session"] == true || value["expires"].as_f64().is_some_and(|expires| expires > now);
        if !live {
            continue;
        }
        let token_value = value["value"]
            .as_str()
            .ok_or_else(|| anyhow!("browser session cookie is invalid"))?;
        ensure!(token(token_value), "browser session cookie is invalid");
        ensure!(
            selected.insert(name, token_value).is_none(),
            "browser session cookies are ambiguous"
        );
    }
    let Some(session) = selected.get("overleaf_session2") else {
        return Ok(None);
    };
    let mut header = format!("overleaf_session2={session}");
    if let Some(lb) = selected.get("GCLB") {
        header.push_str(&format!("; GCLB={lb}"));
    }
    Ok(Some(header))
}

struct Cdp {
    ws: WebSocket<TcpStream>,
    next: u64,
}
impl Cdp {
    fn open(endpoint: &str, deadline: Instant) -> Result<Self> {
        let uri: tungstenite::http::Uri = endpoint
            .parse()
            .map_err(|_| anyhow!("browser debugging address is invalid"))?;
        ensure!(
            uri.scheme_str() == Some("ws")
                && uri.host() == Some("127.0.0.1")
                && uri.port_u16().is_some()
                && uri.path().starts_with("/devtools/browser/")
                && uri.query().is_none(),
            "browser debugging address must be loopback"
        );
        let timeout = remaining(deadline)?.min(Duration::from_secs(2));
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, uri.port_u16().unwrap()));
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|_| anyhow!("cannot connect to browser debugging port"))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| anyhow!("cannot set browser debugging timeout"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| anyhow!("cannot set browser debugging timeout"))?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (ws, _) = tungstenite::client::client_with_config(endpoint, stream, Some(config))
            .map_err(|_| anyhow!("browser debugging handshake failed"))?;
        Ok(Self { ws, next: 0 })
    }
    fn cookies(&mut self, deadline: Instant) -> Result<Value> {
        self.next += 1;
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        self.ws
            .get_mut()
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| anyhow!("cannot set browser debugging timeout"))?;
        self.ws
            .send(Message::Text(
                json!({"id":self.next,"method":"Storage.getCookies","params":{}})
                    .to_string()
                    .into(),
            ))
            .map_err(|_| anyhow!("browser cookie request failed"))?;
        loop {
            self.ws
                .get_mut()
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(|_| anyhow!("cannot set browser debugging timeout"))?;
            let frame = self.ws.read().map_err(|_| anyhow!("browser cookie response failed"))?;
            match frame {
                Message::Text(text) => {
                    let value: Value =
                        serde_json::from_str(&text).map_err(|_| anyhow!("browser debugging response is invalid"))?;
                    if value["id"].as_u64() != Some(self.next) {
                        continue;
                    }
                    ensure!(value.get("error").is_none(), "browser rejected cookie request");
                    return value["result"]
                        .get("cookies")
                        .cloned()
                        .ok_or_else(|| anyhow!("browser cookie response is missing"));
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => bail!("browser debugging connection closed"),
                _ => bail!("browser debugging response is invalid"),
            }
        }
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    ensure!(!duration.is_zero(), "browser login timed out");
    Ok(duration)
}

#[cfg(unix)]
struct Browser {
    child: Child,
    profile: Option<tempfile::TempDir>,
}
#[cfg(unix)]
impl Browser {
    fn start(executable: &Path, target: &str) -> Result<Self> {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        let profile = tempfile::Builder::new()
            .prefix("interleaf-owner-login-")
            .tempdir()
            .map_err(|_| anyhow!("cannot create private browser profile"))?;
        fs::set_permissions(profile.path(), fs::Permissions::from_mode(0o700))
            .map_err(|_| anyhow!("cannot protect private browser profile"))?;
        fs::create_dir(profile.path().join("Default"))
            .map_err(|_| anyhow!("cannot prepare private browser profile"))?;
        fs::write(profile.path().join("Default/Preferences"), br#"{"credentials_enable_service":false,"profile":{"password_manager_enabled":false},"autofill":{"profile_enabled":false,"credit_card_enabled":false}}"#)
            .map_err(|_| anyhow!("cannot prepare private browser profile"))?;
        let child = Command::new(executable)
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args([
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--disable-extensions",
                "--disable-background-networking",
                "--password-store=basic",
                "--use-mock-keychain",
            ])
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|_| anyhow!("cannot launch owner login browser"))?;
        Ok(Self {
            child,
            profile: Some(profile),
        })
    }
    fn running(&mut self) -> Result<()> {
        ensure!(
            self.child
                .try_wait()
                .map_err(|_| anyhow!("cannot inspect login browser"))?
                .is_none(),
            "owner login browser exited"
        );
        Ok(())
    }
    fn endpoint(&self) -> Result<Option<String>> {
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| anyhow!("login browser profile is closed"))?;
        let file = match File::open(profile.path().join("DevToolsActivePort")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => bail!("cannot read browser debugging address"),
        };
        let mut raw = String::new();
        file.take(1025)
            .read_to_string(&mut raw)
            .map_err(|_| anyhow!("browser debugging address is invalid"))?;
        ensure!(raw.len() <= 1024, "browser debugging address exceeds limit");
        let lines: Vec<_> = raw.lines().collect();
        if lines.len() < 2 {
            return Ok(None);
        }
        ensure!(lines.len() == 2, "browser debugging address is invalid");
        let port: u16 = lines[0]
            .parse()
            .map_err(|_| anyhow!("browser debugging port is invalid"))?;
        let path = lines[1];
        ensure!(
            port > 0
                && path.starts_with("/devtools/browser/")
                && path.len() <= 128
                && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b)),
            "browser debugging address is invalid"
        );
        Ok(Some(format!("ws://127.0.0.1:{port}{path}")))
    }
    fn capture(
        &mut self,
        origin: &str,
        expected_email: &str,
        project: Option<&str>,
        deadline: Instant,
    ) -> Result<CapturedSession> {
        let endpoint = loop {
            self.running()?;
            remaining(deadline)?;
            if let Some(endpoint) = self.endpoint()? {
                break endpoint;
            }
            thread::sleep(Duration::from_millis(100).min(remaining(deadline)?));
        };
        let mut cdp = Cdp::open(&endpoint, deadline)?;
        loop {
            self.running()?;
            remaining(deadline)?;
            if let Some(cookie) = cookies_at(&cdp.cookies(deadline)?, origin)? {
                match validate_on(origin, &cookie, project, remaining(deadline)?.min(REQUEST_TIMEOUT)) {
                    Ok(identity) => {
                        ensure!(
                            identity.email.eq_ignore_ascii_case(expected_email),
                            "browser is logged into a different Overleaf account"
                        );
                        return Ok(CapturedSession { cookie, identity });
                    }
                    Err(ValidationError::Expired) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            thread::sleep(Duration::from_millis(300).min(remaining(deadline)?));
        }
    }
    fn close(&mut self) -> Result<()> {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        let Some(profile) = self.profile.take() else {
            return Ok(());
        };
        let group = Pid::from_raw(self.child.id() as i32);
        let mut failed = false;
        if let Err(error) = killpg(group, Signal::SIGTERM) {
            failed |= error != Errno::ESRCH;
        }
        let grace = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < grace => thread::sleep(Duration::from_millis(20)),
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        // Kill the whole private group even if the main browser exited before its children.
        if let Err(error) = killpg(group, Signal::SIGKILL) {
            failed |= error != Errno::ESRCH;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    failed = true;
                    break;
                }
            }
        }
        failed |= profile.close().is_err();
        ensure!(!failed, "cannot completely clean up owner login browser");
        Ok(())
    }
}
#[cfg(unix)]
impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        net::TcpListener,
        process::{Command, Stdio},
        thread,
    };
    const PROJECT: &str = "0123456789abcdef01234567";
    const USER: &str = "1123456789abcdef01234567";

    #[test]
    fn metadata_and_response_status_require_explicit_evidence() {
        let page = format!(
            "<meta content='{{&quot;_id&quot;:&quot;{USER}&quot;,&quot;email&quot;:&quot;owner@example.test&quot;}}' name='ol-user'><meta name='ol-project_id' content='{PROJECT}'>"
        );
        assert_eq!(
            parse_identity(&page, Some(PROJECT)).unwrap(),
            Identity {
                user_id: USER.into(),
                email: "owner@example.test".into()
            }
        );
        assert!(parse_identity(&page, Some(USER)).is_err());
        assert!(parse_identity(&(page.clone() + &page), None).is_err());
        assert!(parse_identity(&page.replace("owner@example.test", "bad email"), None).is_err());
        assert_eq!(status(401, None), Err(ValidationError::Expired));
        assert_eq!(
            status(302, Some("/login?redir=%2Fproject")),
            Err(ValidationError::Expired)
        );
        assert_eq!(
            status(302, Some("https://www.overleaf.com/login")),
            Err(ValidationError::Expired)
        );
        assert_eq!(
            status(302, Some("https://other.test/login")),
            Err(ValidationError::Rejected)
        );
        assert_eq!(status(403, None), Err(ValidationError::Rejected));
        assert_eq!(status(503, None), Err(ValidationError::Unavailable));
        assert_eq!(status(200, None), Ok(()));
        assert_eq!(
            normalize_base_url("https://latex.example/").unwrap(),
            "https://latex.example"
        );
        assert!(normalize_base_url("https://latex.example/project").is_err());
        assert!(normalize_base_url("http://latex.example").is_err());
        assert_eq!(
            normalize_base_url("http://127.0.0.1:3000").unwrap(),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            status_on("https://latex.example", 302, Some("https://latex.example/login")),
            Err(ValidationError::Expired)
        );
        assert_eq!(
            status_on("https://latex.example", 302, Some("https://www.overleaf.com/login")),
            Err(ValidationError::Rejected)
        );
    }

    #[test]
    fn cookie_selection_confines_names_domains_and_values() {
        let session = json!({"name":"overleaf_session2","value":"dummy-session","domain":".overleaf.com","path":"/","secure":true,"session":true,"expires":-1});
        let lb = json!({"name":"GCLB","value":"dummy-lb","domain":"www.overleaf.com","path":"/","secure":true,"session":true,"expires":-1});
        assert_eq!(
            cookies(&json!([session, lb])).unwrap().as_deref(),
            Some("overleaf_session2=dummy-session; GCLB=dummy-lb")
        );
        for domain in [
            "eviloverleaf.com",
            ".overleaf.com.evil.test",
            "other.test",
            "overleaf.com",
        ] {
            let mut wrong = session.clone();
            wrong["domain"] = json!(domain);
            assert_eq!(cookies(&json!([wrong])).unwrap(), None);
        }
        for header in [
            "overleaf_session2=dummy; bad=other",
            "GCLB=only",
            "overleaf_session2=bad\r\nheader",
            "overleaf_session2=a; overleaf_session2=b",
        ] {
            assert_eq!(check_cookie(header), Err(ValidationError::Rejected));
        }
        assert_eq!(check_cookie("overleaf_session2=dummy; GCLB=dummy-lb"), Ok(()));
        let mut bad = session.clone();
        bad["value"] = json!("bad\r\nheader");
        assert!(cookies(&json!([bad])).is_err());
        assert!(cookies(&json!([session, session])).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn cdp_cookie_read_and_browser_cleanup_are_bounded() {
        use std::os::unix::process::CommandExt;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let request: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["method"], "Storage.getCookies");
            ws.send(tungstenite::Message::Text(
                json!({"id":request["id"],"result":{"cookies":[]}}).to_string().into(),
            ))
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut cdp = Cdp::open(&format!("ws://{address}/devtools/browser/dummy"), deadline).unwrap();
        assert_eq!(cdp.cookies(deadline).unwrap(), json!([]));
        peer.join().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let path: PathBuf = profile.path().into();
        let ready = profile.path().join("ready");
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "trap '' TERM; : > \"$1\"; while :; do sleep 1; done",
                "owner-login-test",
            ])
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut browser = Browser {
            child,
            profile: Some(profile),
        };
        let wait = Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(Instant::now() < wait);
            thread::sleep(Duration::from_millis(10));
        }
        let start = Instant::now();
        browser.close().unwrap();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(browser.child.try_wait().unwrap().is_some());
        assert!(!path.exists());
        let profile = tempfile::tempdir().unwrap();
        let path: PathBuf = profile.path().into();
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut browser = Browser {
            child,
            profile: Some(profile),
        };
        thread::sleep(Duration::from_millis(30));
        browser.close().unwrap();
        assert!(!path.exists());
    }
}
