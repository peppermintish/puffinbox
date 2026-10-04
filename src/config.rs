use ipnet::IpNet;
use std::{env, net::SocketAddr, path::PathBuf, str::FromStr};
use url::Url;

#[derive(Clone, Debug, Default)]
pub struct DlnaConfig {
    pub enabled: String,
    pub interface_address: Option<String>,
    pub advertised_origin: Option<String>,
}

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub public_base_url: Option<Url>,
    pub database_url: String,
    pub server_name: String,
    pub web_root: PathBuf,
    pub data_dir: PathBuf,
    pub ffmpeg_path: Option<PathBuf>,
    pub max_scan_workers: usize,
    pub max_page_size: i64,
    pub access_token_lifetime_hours: i64,
    pub cookie_secure: bool,
    pub cors_origins: Vec<String>,
    pub trusted_proxies: Vec<IpNet>,
    pub local_networks: Vec<IpNet>,
    pub dlna: DlnaConfig,
    pub setup_token: Option<String>,
    pub bootstrap_admin_username: Option<String>,
    pub bootstrap_admin_password: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind = parse_bind(env::var("PUFFINBOX_BIND").ok())?;
        let public_base_url = parse_public_base_url(env::var("PUFFINBOX_PUBLIC_BASE_URL").ok())?;
        let database_url = env::var("DATABASE_URL")
            .or_else(|_| env::var("PUFFINBOX_DATABASE_URL"))
            .map_err(|_| "DATABASE_URL is required; PuffinBox does not start with a default database credential".to_owned())?;
        let max_scan_workers = parse_bounded("PUFFINBOX_MAX_SCAN_WORKERS", 4_usize, 1, 64)?;
        let max_page_size = parse_bounded("PUFFINBOX_MAX_PAGE_SIZE", 500_i64, 1, 10_000)?;
        let access_token_lifetime_hours =
            parse_bounded("PUFFINBOX_ACCESS_TOKEN_HOURS", 24_i64 * 30, 1, 24 * 365)?;
        let cookie_secure = env::var("PUFFINBOX_COOKIE_SECURE")
            .map(|v| parse_bool("PUFFINBOX_COOKIE_SECURE", &v))
            .unwrap_or(Ok(false))?;
        let cors_origins = env::var("PUFFINBOX_CORS_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let trusted_proxies = env::var("PUFFINBOX_TRUSTED_PROXIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                IpNet::from_str(s)
                    .map_err(|e| format!("PUFFINBOX_TRUSTED_PROXIES entry {s:?} is invalid: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_https_remote_access_proxy(&public_base_url, &trusted_proxies)?;
        validate_cookie_security(&public_base_url, cookie_secure, &trusted_proxies)?;
        let local_networks = match env::var("PUFFINBOX_LOCAL_NETWORKS") {
            Ok(raw) if !raw.trim().is_empty() => raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    IpNet::from_str(s).map_err(|e| {
                        format!("PUFFINBOX_LOCAL_NETWORKS entry {s:?} is invalid: {e}")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => [
                "127.0.0.0/8",
                "::1/128",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "fc00::/7",
            ]
            .into_iter()
            .map(|s| IpNet::from_str(s).expect("static network CIDR is valid"))
            .collect(),
        };

        let config = Self {
            bind,
            public_base_url,
            database_url,
            server_name: env::var("PUFFINBOX_SERVER_NAME")
                .unwrap_or_else(|_| "PuffinBox".to_owned()),
            web_root: PathBuf::from(
                env::var("PUFFINBOX_WEB_ROOT").unwrap_or_else(|_| "web".to_owned()),
            ),
            data_dir: PathBuf::from(
                env::var("PUFFINBOX_DATA_DIR").unwrap_or_else(|_| "data".to_owned()),
            ),
            ffmpeg_path: env::var("PUFFINBOX_FFMPEG_PATH").ok().map(PathBuf::from),
            max_scan_workers,
            max_page_size,
            access_token_lifetime_hours,
            cookie_secure,
            cors_origins,
            trusted_proxies,
            local_networks,
            dlna: DlnaConfig {
                enabled: env::var("PUFFINBOX_DLNA_ENABLED").unwrap_or_default(),
                interface_address: env::var("PUFFINBOX_DLNA_INTERFACE_ADDRESS").ok(),
                advertised_origin: env::var("PUFFINBOX_DLNA_ADVERTISED_ORIGIN").ok(),
            },
            setup_token: env::var("PUFFINBOX_SETUP_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            bootstrap_admin_username: env::var("PUFFINBOX_BOOTSTRAP_ADMIN_USERNAME")
                .ok()
                .filter(|s| !s.is_empty()),
            bootstrap_admin_password: env::var("PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
        };
        if config.bootstrap_admin_username.is_some() != config.bootstrap_admin_password.is_some() {
            return Err("Set both PUFFINBOX_BOOTSTRAP_ADMIN_USERNAME and PUFFINBOX_BOOTSTRAP_ADMIN_PASSWORD, or neither".to_owned());
        }
        if config.database_url.len() > 4096 {
            return Err("DATABASE_URL is unexpectedly long".to_owned());
        }
        Ok(config)
    }
}

fn parse_public_base_url(raw: Option<String>) -> Result<Option<Url>, String> {
    let Some(raw) = raw.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if raw.len() > 2048 {
        return Err("PUFFINBOX_PUBLIC_BASE_URL exceeds the 2048-character limit".to_owned());
    }
    let url = Url::parse(raw.trim())
        .map_err(|error| format!("PUFFINBOX_PUBLIC_BASE_URL is invalid: {error}"))?;
    let unspecified_host = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_unspecified(),
        Some(url::Host::Ipv6(address)) => address.is_unspecified(),
        _ => false,
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || unspecified_host
        || url.port() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err("PUFFINBOX_PUBLIC_BASE_URL must be an HTTP(S) origin without credentials, path, query, or fragment".to_owned());
    }
    Ok(Some(url))
}

fn parse_bind(raw: Option<String>) -> Result<SocketAddr, String> {
    raw.unwrap_or_else(|| "127.0.0.1:8096".to_owned())
        .parse::<SocketAddr>()
        .map_err(|e| format!("PUFFINBOX_BIND must be a socket address: {e}"))
}

fn validate_cookie_security(
    public_base_url: &Option<Url>,
    cookie_secure: bool,
    trusted_proxies: &[IpNet],
) -> Result<(), String> {
    let https_public_origin = public_base_url
        .as_ref()
        .is_some_and(|url| url.scheme() == "https");
    if (https_public_origin || !trusted_proxies.is_empty()) && !cookie_secure {
        return Err(
            "PUFFINBOX_COOKIE_SECURE must be true when PUFFINBOX_PUBLIC_BASE_URL uses HTTPS or PUFFINBOX_TRUSTED_PROXIES is configured".to_owned(),
        );
    }
    Ok(())
}

fn validate_https_remote_access_proxy(
    public_base_url: &Option<Url>,
    trusted_proxies: &[IpNet],
) -> Result<(), String> {
    let https_public_origin = public_base_url
        .as_ref()
        .is_some_and(|url| url.scheme() == "https");
    if https_public_origin && trusted_proxies.is_empty() {
        return Err(
            "PUFFINBOX_TRUSTED_PROXIES must contain at least one proxy CIDR when PUFFINBOX_PUBLIC_BASE_URL uses HTTPS".to_owned(),
        );
    }
    Ok(())
}

fn parse_bounded<T>(name: &str, default: T, min: T, max: T) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + Copy + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let value = match env::var(name) {
        Ok(raw) => raw
            .parse::<T>()
            .map_err(|e| format!("{name} is invalid: {e}"))?,
        Err(_) => default,
    };
    if value < min || value > max {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(value)
}

fn parse_bool(name: &str, raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(format!("{name} must be true or false")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_bind, parse_public_base_url, validate_cookie_security,
        validate_https_remote_access_proxy,
    };
    use ipnet::IpNet;

    #[test]
    fn validates_public_base_url_as_an_origin() {
        let url = parse_public_base_url(Some("https://media.example:8443/".to_owned()))
            .unwrap()
            .unwrap();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://media.example:8443"
        );
        assert!(parse_public_base_url(None).unwrap().is_none());
    }

    #[test]
    fn standalone_bind_defaults_to_loopback_and_allows_explicit_wildcard() {
        assert_eq!(parse_bind(None).unwrap().to_string(), "127.0.0.1:8096");
        assert_eq!(
            parse_bind(Some("0.0.0.0:8096".to_owned()))
                .unwrap()
                .to_string(),
            "0.0.0.0:8096"
        );
    }

    #[test]
    fn rejects_public_base_url_credentials_paths_and_non_http_schemes() {
        for value in [
            "ftp://media.example",
            "https://user@media.example",
            "https://media.example/admin",
            "https://media.example/?token=secret",
            "https://media.example/#fragment",
            "http://0.0.0.0",
            "http://[::]",
            "http://media.example:0",
        ] {
            assert!(
                parse_public_base_url(Some(value.to_owned())).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn public_https_requires_secure_session_cookies() {
        let public_https = parse_public_base_url(Some("https://media.example".to_owned())).unwrap();
        assert!(validate_cookie_security(&public_https, false, &[]).is_err());
        assert!(validate_cookie_security(&public_https, true, &[]).is_ok());
        let public_http = parse_public_base_url(Some("http://media.example".to_owned())).unwrap();
        assert!(validate_cookie_security(&public_http, false, &[]).is_ok());
    }

    #[test]
    fn trusted_proxy_requires_secure_session_cookies() {
        let no_public_origin = None;
        let trusted_proxy = ["127.0.0.1/32".parse::<IpNet>().unwrap()];
        assert!(validate_cookie_security(&no_public_origin, false, &trusted_proxy).is_err());
        assert!(validate_cookie_security(&no_public_origin, true, &trusted_proxy).is_ok());
    }

    #[test]
    fn public_https_requires_an_explicit_trusted_proxy_cidr() {
        let public_https = parse_public_base_url(Some("https://media.example".to_owned())).unwrap();
        let trusted_proxy = ["10.0.0.0/8".parse::<IpNet>().unwrap()];

        assert!(validate_https_remote_access_proxy(&public_https, &[]).is_err());
        assert!(validate_https_remote_access_proxy(&public_https, &trusted_proxy).is_ok());
        assert!(validate_cookie_security(&public_https, true, &trusted_proxy).is_ok());

        let public_http = parse_public_base_url(Some("http://media.example".to_owned())).unwrap();
        assert!(validate_https_remote_access_proxy(&public_http, &[]).is_ok());
    }
}
