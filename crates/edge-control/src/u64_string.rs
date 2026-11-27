//! Serializes a u64 as a decimal string so it survives JavaScript's 53-bit
//! numbers losslessly. Accepts a number too, for hand-written clients.

use serde::{Deserialize, Deserializer, Serializer, de::Error};

pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Text(String),
        Number(u64),
    }
    match Either::deserialize(deserializer)? {
        Either::Text(text) => text.parse().map_err(D::Error::custom),
        Either::Number(number) => Ok(number),
    }
}

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Wrapper(#[serde(with = "super")] u64);

    #[test]
    fn large_values_are_strings() {
        let json = serde_json::to_string(&Wrapper(u64::MAX)).unwrap();
        assert_eq!(json, "\"18446744073709551615\"");
        assert_eq!(
            serde_json::from_str::<Wrapper>(&json).unwrap(),
            Wrapper(u64::MAX)
        );
        assert_eq!(serde_json::from_str::<Wrapper>("5").unwrap(), Wrapper(5));
    }
}
