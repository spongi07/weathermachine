//! Descriptive User-Agent construction (NWS asks clients to identify themselves).
//!
//! No personal data is compiled in: the contact comes from deployment
//! configuration (`WM_CONTACT`), and NOAA-class collectors refuse to start
//! without it.

use crate::http::NetError;

/// Default template. `{version}` and `{contact}` are substituted.
pub const DEFAULT_TEMPLATE: &str = "WeatherMachine/{version} ({contact})";

/// Build and validate a User-Agent string.
pub fn build_user_agent(template: &str, contact: Option<&str>) -> Result<String, NetError> {
    let contact = contact.map(str::trim).filter(|c| !c.is_empty());
    if template.contains("{contact}") && contact.is_none() {
        return Err(NetError::UserAgent(
            "a contact (e-mail or URL) is required; set WM_CONTACT".into(),
        ));
    }
    let ua = template
        .replace("{version}", wm_core::VERSION)
        .replace("{contact}", contact.unwrap_or_default());
    if ua.is_empty() || ua.len() > 256 {
        return Err(NetError::UserAgent("must be 1..=256 characters".into()));
    }
    if !ua.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(NetError::UserAgent(
            "must be printable ASCII without control characters".into(),
        ));
    }
    Ok(ua)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_from_template() {
        let ua = build_user_agent(DEFAULT_TEMPLATE, Some("ops@example.org")).unwrap();
        assert!(ua.starts_with("WeatherMachine/"));
        assert!(ua.ends_with("(ops@example.org)"));
    }

    #[test]
    fn requires_contact() {
        assert!(build_user_agent(DEFAULT_TEMPLATE, None).is_err());
        assert!(build_user_agent(DEFAULT_TEMPLATE, Some("  ")).is_err());
        assert!(build_user_agent("Static/1.0", None).is_ok());
    }

    #[test]
    fn rejects_header_injection() {
        assert!(build_user_agent("{contact}", Some("a\r\nX-Evil: 1")).is_err());
    }
}
