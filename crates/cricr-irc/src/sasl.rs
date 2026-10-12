//! SASL authentication payload construction (PLAIN and EXTERNAL).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// Supported SASL mechanisms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslMechanism {
    Plain,
    External,
}

impl SaslMechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            SaslMechanism::Plain => "PLAIN",
            SaslMechanism::External => "EXTERNAL",
        }
    }
}

/// Credentials for SASL authentication.
#[derive(Debug, Clone)]
pub struct SaslCredentials {
    pub mechanism: SaslMechanism,
    pub authzid: Option<String>,
    pub authcid: Option<String>,
    pub password: Option<String>,
}

impl SaslCredentials {
    pub fn plain(authcid: impl Into<String>, password: impl Into<String>) -> Self {
        SaslCredentials {
            mechanism: SaslMechanism::Plain,
            authzid: None,
            authcid: Some(authcid.into()),
            password: Some(password.into()),
        }
    }

    pub fn external(authzid: Option<String>) -> Self {
        SaslCredentials {
            mechanism: SaslMechanism::External,
            authzid,
            authcid: None,
            password: None,
        }
    }

    /// The initial client response, already base64 encoded.
    pub fn initial_response(&self) -> String {
        match self.mechanism {
            SaslMechanism::Plain => {
                let authzid = self.authzid.as_deref().unwrap_or("");
                let authcid = self.authcid.as_deref().unwrap_or("");
                let password = self.password.as_deref().unwrap_or("");
                let raw = format!("{authzid}\0{authcid}\0{password}");
                BASE64.encode(raw)
            }
            SaslMechanism::External => match &self.authzid {
                Some(id) => BASE64.encode(id),
                None => String::new(),
            },
        }
    }
}

/// Split a base64 client response into `AUTHENTICATE` command lines.
///
/// SASL responses are transmitted in chunks of at most 400 bytes; a payload
/// whose length is a multiple of 400 must be followed by an empty
/// `AUTHENTICATE +`.
pub fn authenticate_lines(payload: &str) -> Vec<String> {
    if payload.is_empty() {
        return vec!["AUTHENTICATE +".to_string()];
    }
    let bytes = payload.as_bytes();
    let mut lines = Vec::with_capacity(bytes.len() / 400 + 2);
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + 400).min(bytes.len());
        lines.push(format!("AUTHENTICATE {}", &payload[i..end]));
        i = end;
    }
    if bytes.len() % 400 == 0 {
        lines.push("AUTHENTICATE +".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_roundtrip() {
        let creds = SaslCredentials::plain("user", "hunter2");
        let decoded = BASE64.decode(creds.initial_response()).unwrap();
        assert_eq!(decoded, b"\0user\0hunter2");
    }

    #[test]
    fn external_empty() {
        let creds = SaslCredentials::external(None);
        assert_eq!(creds.initial_response(), "");
        assert_eq!(authenticate_lines(""), vec!["AUTHENTICATE +"]);
    }

    #[test]
    fn chunks_at_400() {
        let payload = "a".repeat(400);
        let lines = authenticate_lines(&payload);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), "AUTHENTICATE ".len() + 400);
        assert_eq!(lines[1], "AUTHENTICATE +");
    }

    #[test]
    fn short_payload_single_line() {
        let lines = authenticate_lines("YWJj");
        assert_eq!(lines, vec!["AUTHENTICATE YWJj"]);
    }
}
