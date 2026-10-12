//! IRCv3 capability negotiation helpers.
//!
//! Mirrors `rcirc-implemented-capabilities` in `contrib/rcirc.el`.

/// Capabilities cricr understands and will request when offered.
pub const SUPPORTED: &[&str] = &[
    "message-tags",
    "server-time",
    "batch",
    "message-ids",
    "invite-notify",
    "sasl",
    "multi-prefix",
    "standard-replies",
];

/// Parse a CAP LS/ACK/NAK/LIST parameter list into bare capability names.
///
/// Tokens like `sasl=PLAIN,EXTERNAL` are reduced to `sasl`, and the leading
/// modifiers `-` (disabled) and `~`/`=` (prefix tweaks) are stripped.
pub fn parse_cap_list(tokens: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for token in tokens {
        for raw in token.split(' ') {
            let name = raw
                .trim_start_matches(['-', '~', '='])
                .split('=')
                .next()
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            if !seen.iter().any(|c: &String| c == name) {
                seen.push(name.to_string());
            }
        }
    }
    seen
}

/// Given the capabilities offered by the server, choose the ones to request.
pub fn select_requested(offered: &[String]) -> Vec<String> {
    SUPPORTED
        .iter()
        .filter(|cap| offered.iter().any(|o| o == *cap))
        .map(|cap| cap.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_and_modifiers() {
        let toks = vec![
            "sasl=PLAIN,EXTERNAL".to_string(),
            "-multi-prefix".to_string(),
            "server-time".to_string(),
            "message-tags".to_string(),
        ];
        let caps = parse_cap_list(&toks);
        assert_eq!(
            caps,
            vec!["sasl", "multi-prefix", "server-time", "message-tags"]
        );
    }

    #[test]
    fn selects_intersection() {
        let offered = parse_cap_list(&[
            "sasl".to_string(),
            "server-time".to_string(),
            "unknown-thing".to_string(),
        ]);
        assert_eq!(select_requested(&offered), vec!["server-time", "sasl"]);
    }
}
