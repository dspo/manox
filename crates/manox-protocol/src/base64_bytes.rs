//! serde helper: serialize `Vec<u8>` as a base64 string on JSON boundaries,
//! avoiding the ~4x bloat of a numeric array.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error> {
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(deserializer)?;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Wrap {
        #[serde(with = "super")]
        bytes: Vec<u8>,
    }

    #[test]
    fn base64_round_trips() {
        let w = Wrap {
            bytes: b"hello".to_vec(),
        };
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["bytes"], serde_json::json!("aGVsbG8="));
        let back: Wrap = serde_json::from_value(json).unwrap();
        assert_eq!(w, back);
    }
}
