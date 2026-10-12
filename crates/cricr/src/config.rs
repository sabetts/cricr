//! Standalone TOML configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use cricr_irc::client::{ClientCert, ClientConfig};
use cricr_irc::sasl::{SaslCredentials, SaslMechanism};

use crate::theme::Background;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub defaults: Defaults,
    pub servers: Vec<ServerConfig>,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub nick: String,
    pub user_name: String,
    pub full_name: String,
    pub port: u16,
    pub tls: bool,
    pub password: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            nick: default_nick(),
            user_name: "user".to_string(),
            full_name: "unknown".to_string(),
            port: 6697,
            tls: true,
            password: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub tls: Option<bool>,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub sasl: Option<SaslConfig>,
    #[serde(default)]
    pub cert: Option<PathBuf>,
    #[serde(default)]
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SaslConfig {
    pub mechanism: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub authzid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub timestamp_format: String,
    pub scrollback_lines: usize,
    /// Word-wrap messages at this many columns (0 disables wrapping).
    pub fill_column: usize,
    /// Which rcirc face variant to use: `"dark"` (default) or `"light"`.
    pub background: Background,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            timestamp_format: "%H:%M".to_string(),
            scrollback_lines: 100_000,
            fill_column: 70,
            background: Background::Dark,
        }
    }
}

/// Default configuration file contents written when none exists.
pub const TEMPLATE: &str = r##"# cricr configuration

[defaults]
nick = "yournick"
user_name = "user"
full_name = "unknown"
port = 6697
tls = true

[ui]
timestamp_format = "%H:%M"
scrollback_lines = 100000
fill_column = 70      # word-wrap messages at this column (0 = off)
background = "dark"   # or "light"

[[servers]]
name = "irc.libera.chat"
port = 6697
tls = true
channels = ["#emacs", "#rcirc"]
# password = "server-password"
#
# SASL (recommended for registered nicks):
# sasl = { mechanism = "PLAIN", account = "yournick", password = "secret" }
#
# SASL EXTERNAL with a client certificate (certfp):
# cert = "cert.pem"
# key  = "key.pem"
# sasl = { mechanism = "EXTERNAL" }
"##;

impl Config {
    /// Load configuration from `path`, writing a template if it does not exist.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, TEMPLATE)
                .with_context(|| format!("writing template config to {}", path.display()))?;
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config from {}", path.display()))?;
        Ok(config)
    }

    /// Build the protocol-layer configuration for one server entry.
    pub fn client_config(&self, server: &ServerConfig) -> Result<ClientConfig> {
        let sasl = match &server.sasl {
            Some(s) => {
                let mech = match s.mechanism.to_ascii_uppercase().as_str() {
                    "EXTERNAL" => SaslMechanism::External,
                    other => {
                        debug_assert_eq!(other, "PLAIN");
                        SaslMechanism::Plain
                    }
                };
                Some(match mech {
                    SaslMechanism::Plain => SaslCredentials {
                        mechanism: mech,
                        authzid: s.authzid.clone(),
                        authcid: s
                            .account
                            .clone()
                            .or_else(|| Some(server.nick_or(&self.defaults))),
                        password: s.password.clone(),
                    },
                    SaslMechanism::External => SaslCredentials {
                        mechanism: mech,
                        authzid: s.authzid.clone(),
                        authcid: None,
                        password: None,
                    },
                })
            }
            None => None,
        };

        let client_cert = match (&server.cert, &server.key) {
            (Some(cert), Some(key)) => Some(ClientCert {
                cert_pem: std::fs::read(cert)
                    .with_context(|| format!("reading certificate {}", cert.display()))?,
                key_pem: std::fs::read(key)
                    .with_context(|| format!("reading key {}", key.display()))?,
            }),
            _ => None,
        };

        Ok(ClientConfig {
            server: server.name.clone(),
            port: server.port.unwrap_or(self.defaults.port),
            tls: server.tls.unwrap_or(self.defaults.tls),
            nick: server.nick_or(&self.defaults),
            user_name: server
                .user_name
                .clone()
                .unwrap_or_else(|| self.defaults.user_name.clone()),
            full_name: server
                .full_name
                .clone()
                .unwrap_or_else(|| self.defaults.full_name.clone()),
            server_password: server
                .password
                .clone()
                .or_else(|| self.defaults.password.clone()),
            sasl,
            client_cert,
            channels: server.channels.clone(),
        })
    }
}

impl ServerConfig {
    fn nick_or(&self, defaults: &Defaults) -> String {
        self.nick.clone().unwrap_or_else(|| defaults.nick.clone())
    }
}

fn default_nick() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "cricr".to_string())
}

/// The default location of the configuration file for this platform.
pub fn default_path() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("", "", "cricr") {
        proj.config_dir().join("cricr.toml")
    } else {
        PathBuf::from("cricr.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_template() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "irc.libera.chat");
        assert_eq!(cfg.servers[0].channels, vec!["#emacs", "#rcirc"]);
        assert_eq!(cfg.ui.scrollback_lines, 100_000);
        assert_eq!(cfg.ui.fill_column, 70);
        assert_eq!(cfg.ui.background, Background::Dark);
    }

    #[test]
    fn defaults_apply() {
        let cfg: Config = toml::from_str("[[servers]]\nname = \"irc.example.org\"\n").unwrap();
        let cc = cfg.client_config(&cfg.servers[0]).unwrap();
        assert_eq!(cc.port, 6697);
        assert!(cc.tls);
        assert_eq!(cc.nick, cfg.defaults.nick);
    }

    #[test]
    fn sasl_plain_uses_account() {
        let toml = r#"
[[servers]]
name = "irc.example.org"
nick = "me"
sasl = { mechanism = "PLAIN", password = "secret" }
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let cc = cfg.client_config(&cfg.servers[0]).unwrap();
        let sasl = cc.sasl.unwrap();
        assert_eq!(sasl.mechanism.as_str(), "PLAIN");
        assert_eq!(sasl.authcid.as_deref(), Some("me"));
        assert_eq!(sasl.password.as_deref(), Some("secret"));
    }
}
