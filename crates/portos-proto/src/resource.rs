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
#[serde(into = "ReleaseWire", try_from = "ReleaseWire")]
pub enum ReleaseResponse {
    Released,
    Pending { cleanup_id: u64 },
    Retryable { cleanup_id: u64, reason: String },
    Unknown { cleanup_id: u64, reason: String },
    Blocked { cleanup_id: u64, reason: String },
}
impl ReleaseResponse {
    pub fn is_released(&self) -> bool {
        matches!(self, Self::Released)
    }
}
#[derive(Serialize, Deserialize)]
struct ReleaseWire {
    released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}
impl From<ReleaseResponse> for ReleaseWire {
    fn from(r: ReleaseResponse) -> Self {
        let (state, cleanup_id, reason) = match r {
            ReleaseResponse::Released => ("retired", None, None),
            ReleaseResponse::Pending { cleanup_id } => ("pending", Some(cleanup_id), None),
            ReleaseResponse::Retryable { cleanup_id, reason } => {
                ("retryable", Some(cleanup_id), Some(reason))
            }
            ReleaseResponse::Unknown { cleanup_id, reason } => {
                ("unknown", Some(cleanup_id), Some(reason))
            }
            ReleaseResponse::Blocked { cleanup_id, reason } => {
                ("blocked", Some(cleanup_id), Some(reason))
            }
        };
        Self {
            released: state == "retired",
            state: Some(state.into()),
            cleanup_id,
            reason,
        }
    }
}
impl TryFrom<ReleaseWire> for ReleaseResponse {
    type Error = &'static str;
    fn try_from(w: ReleaseWire) -> Result<Self, Self::Error> {
        match (w.released, w.state.as_deref(), w.cleanup_id, w.reason) {
            (true, None | Some("retired"), None, None) => Ok(Self::Released),
            (false, Some("pending"), Some(cleanup_id), None) => Ok(Self::Pending { cleanup_id }),
            (false, Some("retryable"), Some(cleanup_id), Some(reason)) => {
                Ok(Self::Retryable { cleanup_id, reason })
            }
            (false, Some("unknown"), Some(cleanup_id), Some(reason)) => {
                Ok(Self::Unknown { cleanup_id, reason })
            }
            (false, Some("blocked"), Some(cleanup_id), Some(reason)) => {
                Ok(Self::Blocked { cleanup_id, reason })
            }
            _ => Err("inconsistent release result"),
        }
    }
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
    #[test]
    fn pending_cleanup_cannot_decode_as_a_successful_release() {
        let results = [
            ReleaseResponse::Released,
            ReleaseResponse::Pending { cleanup_id: 0 },
            ReleaseResponse::Retryable {
                cleanup_id: 1,
                reason: "busy".into(),
            },
            ReleaseResponse::Unknown {
                cleanup_id: 2,
                reason: "lost response".into(),
            },
            ReleaseResponse::Blocked {
                cleanup_id: 3,
                reason: "provider unavailable".into(),
            },
        ];
        for result in results {
            let wire = serde_json::to_value(&result).unwrap();
            assert_eq!(wire["released"], result.is_released());
            assert_eq!(
                serde_json::from_value::<ReleaseResponse>(wire).unwrap(),
                result
            );
        }
        assert_eq!(
            serde_json::from_value::<ReleaseResponse>(json!({"released":true})).unwrap(),
            ReleaseResponse::Released
        );
        for bad in [
            json!({"released":true,"state":"pending","cleanup_id":0}),
            json!({"released":false}),
            json!({"released":false,"state":"blocked","cleanup_id":1}),
        ] {
            assert!(serde_json::from_value::<ReleaseResponse>(bad).is_err());
        }
    }
}
