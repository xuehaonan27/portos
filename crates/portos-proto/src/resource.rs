//! Resource wire messages. Domain existence, incarnation and ownership checks
//! belong to the kernel; absence of required fields is a decoding error.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldingRef {
    pub id: u64,
    #[serde(deserialize_with = "nonempty")]
    pub generation: String,
}
fn nonempty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.is_empty() {
        return Err(serde::de::Error::custom("generation must not be empty"));
    }
    Ok(s)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HoldRequest {
    pub class: String,
    pub instance: String,
    pub substrate: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRequest {
    #[serde(flatten)]
    pub holding: HoldingRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewRequest {
    #[serde(flatten)]
    pub holding: HoldingRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_secs: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ResourceRequest {
    Hold(HoldRequest),
    Release(ReleaseRequest),
    Renew(RenewRequest),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseResponse {
    pub released: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewResponse {
    #[serde(deserialize_with = "required_expiry")]
    pub lease_expires_at: Option<u64>,
}

fn required_expiry<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn existing_requests_round_trip_and_missing_identity_is_rejected() {
        let cases = [
            json!({"op":"hold","class":"kernel/port","instance":"tcp:1234","substrate":{}}),
            json!({"op":"release","id":0,"generation":"旧世代"}),
            json!({"op":"renew","id":3,"generation":"g","lease_secs":0}),
        ];
        for value in cases {
            let request: ResourceRequest = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), value);
        }
        for value in [
            json!({"op":"release","generation":"g"}),
            json!({"op":"renew","id":0}),
            json!({"op":"release","id":0,"generation":""}),
            json!({"op":"hold","class":"x","instance":"i"}),
            json!({"op":"renew","id":0,"generation":"g","lease_secs":"2"}),
        ] {
            assert!(serde_json::from_value::<ResourceRequest>(value).is_err());
        }
    }
    #[test]
    fn responses_require_fields_even_when_expiry_is_null() {
        assert!(serde_json::from_value::<HoldingRef>(json!({"id":0})).is_err());
        assert!(serde_json::from_value::<ReleaseResponse>(json!({})).is_err());
        assert!(serde_json::from_value::<RenewResponse>(json!({})).is_err());
        assert_eq!(
            serde_json::from_value::<RenewResponse>(json!({"lease_expires_at":null}))
                .unwrap()
                .lease_expires_at,
            None
        );
    }
}
