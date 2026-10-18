use super::replay::{Result, keys, parse_json};
use serde_json::{Value, json};
use std::path::Path;
use url::Url;

#[derive(Clone)]
pub(crate) struct Config {
    pub enabled: bool,
    pub base_url: Url,
    pub token_url: Url,
    pub authorization_url: Url,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub project: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: Url::parse("https://cloudcode-pa.googleapis.com").unwrap(),
            token_url: Url::parse("https://oauth2.googleapis.com/token").unwrap(),
            authorization_url: Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap(),
            client_id: None,
            client_secret: None,
            project: None,
        }
    }
}

impl Config {
    pub fn validate_endpoint(raw: &str, test_loopback: bool) -> Result<Url> {
        let url = Url::parse(raw).map_err(|_| "invalid Antigravity endpoint")?;
        let loopback = url.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !(url.scheme() == "https" || (test_loopback && loopback && url.scheme() == "http"))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err("invalid Antigravity endpoint");
        }
        Ok(url)
    }

    pub fn load() -> Result<Self> {
        Self::read(&crate::paths::config_dir().join("config.json"), |key| {
            std::env::var(key).ok()
        })
    }

    pub fn read(path: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let file = match std::fs::read(path) {
            Ok(raw) => {
                parse_json(&raw, 1024 * 1024).map_err(|_| "invalid Antigravity configuration")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(_) => return Err("unreadable Antigravity configuration"),
        };
        if !file.is_object() {
            return Err("invalid Antigravity configuration root");
        }
        let section = file
            .get("antigravity")
            .cloned()
            .unwrap_or_else(|| json!({}));
        keys(
            &section,
            &[
                "enabled",
                "baseUrl",
                "clientId",
                "clientSecret",
                "projectId",
            ],
            &[],
        )?;
        let mut result = Self::default();
        if let Some(enabled) = section.get("enabled") {
            result.enabled = enabled.as_bool().ok_or("invalid enabled flag")?;
        }
        if let Some(enabled) = env("CCP_ANTIGRAVITY_ENABLED") {
            result.enabled = match enabled.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                _ => return Err("invalid enabled flag"),
            };
        }
        fn setting(section: &Value, key: &str, supplied: Option<String>) -> Result<Option<String>> {
            let value = supplied.or(section
                .get(key)
                .map(|v| v.as_str().unwrap_or("").to_owned()));
            if value
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 4096 || v.chars().any(char::is_control))
            {
                return Err("invalid Antigravity setting");
            }
            Ok(value)
        }
        if let Some(base) = setting(&section, "baseUrl", env("CCP_ANTIGRAVITY_BASE_URL"))? {
            result.base_url = Self::validate_endpoint(&base, false)?;
            if result.base_url.path() != "/" {
                return Err("Antigravity base URL must be an origin");
            }
        }
        result.client_id = setting(&section, "clientId", env("CCP_ANTIGRAVITY_CLIENT_ID"))?;
        result.client_secret = setting(
            &section,
            "clientSecret",
            env("CCP_ANTIGRAVITY_CLIENT_SECRET"),
        )?;
        result.project = setting(&section, "projectId", env("CCP_ANTIGRAVITY_PROJECT_ID"))?;
        Ok(result)
    }

    pub fn client_builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(180))
            .user_agent(format!(
                "claude-code-proxy/{} (Antigravity-compatible; {}; {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
    }

    pub fn client(&self) -> Result<reqwest::Client> {
        self.client_builder()
            .build()
            .map_err(|_| "Antigravity HTTP client initialization failed")
    }
}
