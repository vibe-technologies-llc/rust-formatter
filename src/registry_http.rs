use std::{
    fmt, fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ureq::{
    Agent, Proxy,
    tls::{Certificate, PemItem, RootCerts, TlsConfig, parse_pem},
};

/// A sparse index body is plain JSON Lines; the largest on crates.io are a few
/// megabytes, so this is a guard against a hostile mirror, not a real bound.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Redirects are not part of the sparse protocol, so one or two hops are an
/// accommodation for a mirror in front of it and not a path to be walked far.
const MAX_REDIRECTS: usize = 2;

const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(10);
/// A `Retry-After` far in the future would hold a formatter run open; past this
/// the header is treated as a refusal rather than a delay.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub timeout: Duration,
    pub retries: u32,
    pub proxy: Option<String>,
    pub cainfo: Option<PathBuf>,
    pub concurrency: usize,
    /// Set only for a loopback base URL, which is the one case where a cleartext
    /// registry cannot be an intercepted remote one.
    pub allow_cleartext: bool,
    pub user_agent: String,
}

#[derive(Debug)]
pub enum HttpError {
    Transport(String),
    Status {
        code: u16,
        retry_after: Option<Duration>,
    },
    Tls(String),
    Redirect(String),
}

impl HttpError {
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    #[cfg(test)]
    fn status(code: u16) -> Self {
        Self::Status {
            code,
            retry_after: None,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(details) => write!(f, "{details}"),
            Self::Status { code, .. } => write!(f, "http status {code}"),
            Self::Tls(details) => write!(
                f,
                "TLS handshake failed: {details}\n\
                 If a proxy re-signs TLS traffic, point CARGO_HTTP_CAINFO or SSL_CERT_FILE at its \
                 certificate bundle."
            ),
            Self::Redirect(details) => write!(f, "refused a redirect: {details}"),
        }
    }
}

#[derive(Debug)]
pub enum Fetched {
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
    NotModified,
    NotFound,
}

pub struct HttpClient {
    agent: Agent,
    retries: u32,
    user_agent: String,
    allow_cleartext: bool,
}

impl HttpClient {
    pub fn new(options: &HttpOptions) -> Result<Self, HttpError> {
        let idle = options.concurrency.max(1);
        let config = Agent::config_builder()
            .timeout_global(Some(options.timeout))
            .timeout_connect(Some(options.timeout.min(Duration::from_secs(10))))
            .https_only(!options.allow_cleartext)
            // Redirects and status handling are done here rather than by ureq:
            // a 404 must be a value the caller can classify as a skip, and a
            // redirect must be checked against the host it came from.
            .max_redirects(0)
            .http_status_as_error(false)
            .max_idle_connections(idle)
            .max_idle_connections_per_host(idle)
            .tls_config(tls_config(options.cainfo.as_deref())?)
            .proxy(proxy(options.proxy.as_deref())?)
            .build();

        Ok(Self {
            agent: config.into(),
            retries: options.retries,
            user_agent: options.user_agent.clone(),
            allow_cleartext: options.allow_cleartext,
        })
    }

    pub fn get(&self, url: &str, etag: Option<&str>) -> Result<Fetched, HttpError> {
        let mut attempt = 0;
        loop {
            match self.follow(url, etag) {
                Ok(fetched) => return Ok(fetched),
                Err(err) if attempt < self.retries && retry_delay(&err).is_some() => {
                    let hinted = retry_delay(&err).unwrap_or_default();
                    thread::sleep(hinted.max(backoff(attempt)));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn follow(&self, url: &str, etag: Option<&str>) -> Result<Fetched, HttpError> {
        let mut target = url.to_owned();

        for _ in 0..=MAX_REDIRECTS {
            let mut request = self
                .agent
                .get(target.as_str())
                .header("User-Agent", self.user_agent.as_str())
                .header("Accept", "text/plain");
            if let Some(etag) = etag {
                request = request.header("If-None-Match", etag);
            }

            let mut response = request.call().map_err(transport_error)?;
            let status = response.status().as_u16();

            match status {
                200 => {
                    let etag = response
                        .headers()
                        .get("etag")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(MAX_BODY_BYTES)
                        .read_to_vec()
                        .map_err(transport_error)?;
                    return Ok(Fetched::Body { bytes, etag });
                }
                304 => return Ok(Fetched::NotModified),
                404 | 410 => return Ok(Fetched::NotFound),
                301 | 302 | 303 | 307 | 308 => {
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| {
                            HttpError::Redirect(format!("{status} without a Location header"))
                        })?;
                    target = self.redirect_target(&target, location)?;
                }
                other => {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(retry_after);
                    return Err(HttpError::Status {
                        code: other,
                        retry_after,
                    });
                }
            }
        }

        Err(HttpError::Redirect(format!(
            "more than {MAX_REDIRECTS} hops from {url}"
        )))
    }

    fn redirect_target(&self, from: &str, location: &str) -> Result<String, HttpError> {
        let (scheme, host) = split_origin(location)
            .ok_or_else(|| HttpError::Redirect(format!("{location} is not an absolute URL")))?;
        let (_, origin) = split_origin(from)
            .ok_or_else(|| HttpError::Redirect(format!("{from} is not an absolute URL")))?;

        if scheme != "https" && !(self.allow_cleartext && scheme == "http") {
            return Err(HttpError::Redirect(format!("{location} is not https")));
        }
        if !host.eq_ignore_ascii_case(origin) {
            return Err(HttpError::Redirect(format!(
                "{location} leaves the registry host {origin}"
            )));
        }
        Ok(location.to_owned())
    }
}

fn transport_error(err: ureq::Error) -> HttpError {
    match err {
        ureq::Error::Tls(_) | ureq::Error::Pem(_) | ureq::Error::Rustls(_) => {
            HttpError::Tls(err.to_string())
        }
        other => HttpError::Transport(other.to_string()),
    }
}

/// Whether a failure is worth another attempt: a refusal by the server to answer
/// *now*, rather than an answer this tool does not like.
fn retry_delay(err: &HttpError) -> Option<Duration> {
    match err {
        HttpError::Transport(_) => Some(Duration::ZERO),
        HttpError::Status { code, retry_after }
            if matches!(code, 408 | 429) || (500..600).contains(code) =>
        {
            Some(retry_after.unwrap_or_default())
        }
        _ => None,
    }
}

fn backoff(attempt: u32) -> Duration {
    let scaled = BACKOFF_BASE
        .checked_mul(1u32 << attempt.min(16))
        .unwrap_or(BACKOFF_CAP)
        .min(BACKOFF_CAP);
    // Full jitter, so a workspace whose crates all fail at once does not retry
    // them in lockstep. The clock is seed enough for spreading sleeps.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()));
    // The cap keeps `scaled` far below a u64 of nanoseconds; saturating rather
    // than truncating is still the only answer that cannot invert the delay.
    let span = u64::try_from(scaled.as_nanos().max(1)).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos % span)
}

/// `Retry-After` as delta-seconds; the HTTP-date form is treated as "wait the
/// capped maximum" rather than parsed, since the two spellings mean the same
/// thing to a caller that only needs a delay.
fn retry_after(value: &str) -> Option<Duration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<u64>() {
        Ok(seconds) => Some(Duration::from_secs(seconds).min(RETRY_AFTER_CAP)),
        Err(_) => Some(RETRY_AFTER_CAP),
    }
}

fn tls_config(cainfo: Option<&Path>) -> Result<TlsConfig, HttpError> {
    let builder = TlsConfig::builder();
    let Some(path) = cainfo else {
        // The OS trust store, which is what libcurl gives cargo. A corporate
        // proxy's root is installed there and nowhere this tool could bundle.
        return Ok(builder.root_certs(RootCerts::PlatformVerifier).build());
    };

    let pem = fs::read(path)
        .map_err(|err| HttpError::Tls(format!("{} could not be read: {err}", path.display())))?;
    let certs: Vec<Certificate<'static>> = parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(PemItem::Certificate(cert)) => Some(cert),
            _ => None,
        })
        .collect();
    if certs.is_empty() {
        return Err(HttpError::Tls(format!(
            "{} holds no PEM certificates",
            path.display()
        )));
    }
    Ok(builder
        .root_certs(RootCerts::Specific(certs.into()))
        .build())
}

fn proxy(configured: Option<&str>) -> Result<Option<Proxy>, HttpError> {
    match configured {
        Some(url) => Proxy::new(url)
            .map(Some)
            .map_err(|err| HttpError::Transport(format!("proxy {url} is unusable: {err}"))),
        None => Ok(Proxy::try_from_env().map(Some).unwrap_or_default()),
    }
}

fn split_origin(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() {
        return None;
    }
    Some((scheme, host))
}

/// Whether `base` is a plain-HTTP URL pointing at this machine. Only those relax
/// `https_only`, so an override can never downgrade a remote fetch to cleartext.
pub fn is_loopback_http(base: &str) -> bool {
    let Some((scheme, authority)) = split_origin(base) else {
        return false;
    };
    if scheme != "http" {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => authority.split(':').next().unwrap_or(authority),
    };

    host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || host.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> HttpOptions {
        HttpOptions {
            timeout: Duration::from_secs(30),
            retries: 3,
            proxy: None,
            cainfo: None,
            concurrency: 4,
            allow_cleartext: false,
            user_agent: "rust-formatter/test".to_owned(),
        }
    }

    #[test]
    fn loopback_http_bases_are_the_only_cleartext_exception() {
        for base in [
            "http://127.0.0.1:8080",
            "http://127.1.2.3",
            "http://localhost:9000/index",
            "http://[::1]:9000",
        ] {
            assert!(is_loopback_http(base), "{base}");
        }

        for base in [
            "https://index.crates.io",
            "http://192.0.2.1",
            "http://localhost.example.com",
            "http://user@example.com",
            "ftp://127.0.0.1",
            "not-a-url",
        ] {
            assert!(!is_loopback_http(base), "{base}");
        }
    }

    #[test]
    fn a_redirect_must_stay_on_the_same_host_and_on_https() {
        let client = HttpClient::new(&options()).unwrap();
        let from = "https://index.crates.io/se/rd/serde";

        assert_eq!(
            client
                .redirect_target(from, "https://index.crates.io/se/rd/serde2")
                .unwrap(),
            "https://index.crates.io/se/rd/serde2"
        );
        for bad in [
            "http://index.crates.io/se/rd/serde",
            "https://evil.example.com/se/rd/serde",
            "/se/rd/serde",
        ] {
            assert!(client.redirect_target(from, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_loopback_client_may_follow_a_cleartext_redirect_to_itself() {
        let mut options = options();
        options.allow_cleartext = true;
        let client = HttpClient::new(&options).unwrap();

        assert!(
            client
                .redirect_target("http://127.0.0.1:8080/a", "http://127.0.0.1:8080/b")
                .is_ok()
        );
        assert!(
            client
                .redirect_target("http://127.0.0.1:8080/a", "http://192.0.2.1/b")
                .is_err()
        );
    }

    #[test]
    fn only_a_server_saying_not_now_is_retried() {
        assert!(retry_delay(&HttpError::Transport("reset".into())).is_some());
        assert!(retry_delay(&HttpError::status(429)).is_some());
        assert!(retry_delay(&HttpError::status(503)).is_some());
        assert!(retry_delay(&HttpError::status(403)).is_none());
        assert_eq!(
            retry_delay(&HttpError::Status {
                code: 429,
                retry_after: Some(Duration::from_secs(2)),
            }),
            Some(Duration::from_secs(2))
        );
        assert!(retry_delay(&HttpError::Tls("bad root".into())).is_none());
        assert!(retry_delay(&HttpError::Redirect("off host".into())).is_none());
    }

    #[test]
    fn backoff_grows_but_stays_under_the_cap() {
        for attempt in 0..12 {
            assert!(backoff(attempt) <= BACKOFF_CAP, "{attempt}");
        }
    }

    #[test]
    fn retry_after_reads_seconds_and_caps_a_date() {
        assert_eq!(retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(retry_after("99999"), Some(RETRY_AFTER_CAP));
        assert_eq!(
            retry_after("Wed, 21 Oct 2026 07:28:00 GMT"),
            Some(RETRY_AFTER_CAP)
        );
        assert_eq!(retry_after(""), None);
    }

    #[test]
    fn a_ca_bundle_without_certificates_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("roots.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        assert!(tls_config(Some(&empty)).is_err());
        assert!(tls_config(Some(&dir.path().join("missing.pem"))).is_err());
    }

    #[test]
    fn an_origin_is_split_from_scheme_and_authority() {
        assert_eq!(
            split_origin("https://example.com/a/b?c#d"),
            Some(("https", "example.com"))
        );
        assert_eq!(
            split_origin("https://user:pw@example.com:8443/a"),
            Some(("https", "example.com:8443"))
        );
        assert_eq!(split_origin("example.com/a"), None);
        assert_eq!(split_origin("https:///a"), None);
    }
}
