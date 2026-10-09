//! Protocol-version negotiation.
//!
//! AHP has the client offer a list and the host answer with the highest entry
//! inside its supported caret ranges; a host that cannot speak any of them
//! answers `UnsupportedProtocolVersion` (-32005) with the versions it does
//! speak, and the client is expected to report that to its user rather than
//! retry blindly. Selection itself lives in `ahp-types` — keeping this module
//! a thin adapter is what makes an upgrade a single edit (the `ahp-types`
//! pin) instead of a hunt.

use crate::error::HostError;

/// The version this build speaks, advertised as the preferred entry.
pub fn preferred() -> &'static str {
    ahp_types::version::PROTOCOL_VERSION
}

/// Every version this build can speak, most preferred first.
pub fn supported() -> &'static [&'static str] {
    ahp_types::version::SUPPORTED_PROTOCOL_VERSIONS
}

/// Pick the highest offered version inside a supported caret range.
///
/// The choice (and the malformed-offer rejection) is the SDK's own
/// [`ahp_types::version::negotiate_protocol_version`]; the host answers with
/// the same string, and both peers use it for the rest of the connection. A
/// malformed offer is refused exactly like an unsupported one — the wire
/// contract has a single failure shape (`-32005` plus the supported list).
pub fn negotiate(offered: &[String]) -> Result<String, HostError> {
    let refuse = || HostError::UnsupportedProtocolVersion {
        offered: offered.to_vec(),
    };
    match ahp_types::version::negotiate_protocol_version(offered) {
        Ok(Some(version)) => Ok(version.to_string()),
        _ => Err(refuse()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baselines this host speaks are exactly the ones the SDK advertises;
    /// pin them so an upstream narrowing or widening fails here first, not in
    /// a field connection (the ≤0.8 baselines were dropped in 1.0.0).
    #[test]
    fn the_supported_baselines_are_exactly_the_official_two() {
        assert_eq!(supported(), &["1.0.0", "0.9.0"]);
    }

    #[test]
    fn picks_the_highest_caret_compatible_offer_regardless_of_order() {
        assert_eq!(
            negotiate(&["0.9.0".to_string(), "1.0.0".to_string()]).expect("negotiated"),
            "1.0.0"
        );
    }

    #[test]
    fn refuses_when_nothing_overlaps_and_reports_what_it_speaks() {
        let error = negotiate(&["9.9.9".to_string()]).expect_err("must refuse");
        let data = error.data().expect("the supported list is required");
        let versions = data["supportedVersions"].as_array().expect("an array");
        assert!(versions.iter().any(|value| value == preferred()));
    }

    #[test]
    fn refuses_malformed_offers_through_the_same_error_shape() {
        let error = negotiate(&["1.0".to_string()]).expect_err("must refuse");
        let data = error.data().expect("the supported list is required");
        assert!(data["supportedVersions"].as_array().is_some());
    }
}
