//! Protocol-version negotiation.
//!
//! AHP has the client offer an ordered list and the host pick one; a host that
//! cannot speak any of them answers `UnsupportedProtocolVersion` (-32005) with
//! the versions it does speak, and the client is expected to report that to its
//! user rather than retry blindly. Keeping the choice in one function is what
//! makes an upgrade a single edit (the `ahp-types` pin) instead of a hunt.

use crate::error::HostError;

/// The version this build speaks, advertised as the preferred entry.
pub fn preferred() -> &'static str {
    ahp_types::version::PROTOCOL_VERSION
}

/// Every version this build can speak, most preferred first.
pub fn supported() -> &'static [&'static str] {
    ahp_types::version::SUPPORTED_PROTOCOL_VERSIONS
}

/// Pick the first version the client offered that this build speaks.
///
/// The client's order is its preference, so the first match wins; the host
/// answers with the same string, and both peers use it for the rest of the
/// connection.
pub fn negotiate(offered: &[String]) -> Result<String, HostError> {
    offered
        .iter()
        .find(|version| supported().contains(&version.as_str()))
        .cloned()
        .ok_or_else(|| HostError::UnsupportedProtocolVersion {
            offered: offered.to_vec(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_clients_first_acceptable_offer() {
        assert_eq!(
            negotiate(&["9.9.9".to_string(), preferred().to_string()]).expect("negotiated"),
            preferred()
        );
    }

    #[test]
    fn refuses_when_nothing_overlaps_and_reports_what_it_speaks() {
        let error = negotiate(&["9.9.9".to_string()]).expect_err("must refuse");
        let data = error.data().expect("the supported list is required");
        let versions = data["supportedVersions"].as_array().expect("an array");
        assert!(versions.iter().any(|value| value == preferred()));
    }
}
