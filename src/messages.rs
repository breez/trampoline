use cln_rpc::primitives::ShortChannelId;
use serde::{Deserialize, Serialize, Serializer};

use crate::tlv::SerializedTlvStream;

#[derive(Debug, Deserialize, PartialEq)]
pub struct HtlcAcceptedRequest {
    pub onion: Onion,
    pub htlc: Htlc,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Onion {
    pub payload: SerializedTlvStream,
    pub short_channel_id: Option<ShortChannelId>,
    pub forward_msat: u64,
    pub outgoing_cltv_value: u32,
    pub total_msat: Option<u64>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Htlc {
    pub short_channel_id: ShortChannelId,
    pub id: u64,
    pub amount_msat: u64,
    pub cltv_expiry: u32,
    pub cltv_expiry_relative: i64,
    #[serde(with = "hex::serde")]
    pub payment_hash: Vec<u8>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "result")]
pub enum HtlcAcceptedResponse {
    #[serde(rename = "continue")]
    Continue {
        #[serde(default)]
        #[serde(skip_serializing_if = "Option::is_none", serialize_with = "to_hex")]
        payload: Option<Vec<u8>>,
    },
    #[serde(rename = "fail")]
    Fail {
        #[serde(with = "hex::serde")]
        failure_message: Vec<u8>,
    },
    #[serde(rename = "resolve")]
    Resolve {
        #[serde(with = "hex::serde")]
        payment_key: Vec<u8>,
    },
}

impl HtlcAcceptedResponse {
    pub fn temporary_node_failure() -> Self {
        HtlcAcceptedResponse::Fail {
            failure_message: HtlcFailReason::TemporaryNodeFailure.encode(),
        }
    }

    pub fn temporary_trampoline_failure(policy: TrampolineRoutingPolicy) -> Self {
        HtlcAcceptedResponse::Fail {
            failure_message: HtlcFailReason::TemporaryTrampolineFailure(policy).encode(),
        }
    }
}

impl std::fmt::Debug for HtlcAcceptedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Continue { payload } => match payload {
                Some(payload) => write!(f, "continue {{ payload: {} }}", hex::encode(payload)),
                None => write!(f, "continue"),
            },
            Self::Fail { failure_message } => write!(
                f,
                "fail {{ failure_message: {} }}",
                hex::encode(failure_message)
            ),
            Self::Resolve { payment_key: _ } => write!(f, "resolve {{ payment_key: redacted }}"),
        }
    }
}

/// Serializes `buffer` to a lowercase hex string.
fn to_hex<T, S>(buffer: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    T: AsRef<[u8]>,
    S: Serializer,
{
    match buffer {
        None => serializer.serialize_none(),
        Some(buffer) => serializer.serialize_str(&hex::encode(buffer.as_ref())),
    }
}

pub enum HtlcFailReason {
    TemporaryNodeFailure,
    TemporaryTrampolineFailure(TrampolineRoutingPolicy),
}

impl HtlcFailReason {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            HtlcFailReason::TemporaryNodeFailure => {
                vec![0x20, 2]
            }
            HtlcFailReason::TemporaryTrampolineFailure(policy) => {
                let mut s = vec![0x20, 26];
                s.extend_from_slice(&policy.fee_base_msat.to_be_bytes());
                s.extend_from_slice(&policy.fee_proportional_millionths.to_be_bytes());
                s.extend_from_slice(&policy.cltv_expiry_delta.to_be_bytes());
                s
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrampolineRoutingPolicy {
    pub fee_base_msat: u32,
    pub fee_proportional_millionths: u32,
    pub cltv_expiry_delta: u16,
}

impl TrampolineRoutingPolicy {
    pub fn fee_sufficient(&self, total_msat: u64, invoice_msat: u64) -> bool {
        if total_msat < invoice_msat {
            return false;
        }

        let rate_part = match invoice_msat.checked_mul(self.fee_proportional_millionths as u64) {
            Some(rate_part) => rate_part / 1_000_000,
            None => return false,
        };

        let fee_msat = match (self.fee_base_msat as u64).checked_add(rate_part) {
            Some(total_part) => total_part,
            None => return false,
        };

        total_msat >= invoice_msat + fee_msat
    }
}

#[derive(Debug, Deserialize)]
pub struct BlockAddedNotification {
    pub block_added: BlockAdded,
}

#[derive(Debug, Deserialize)]
pub struct BlockAdded {
    // pub hash: String,
    pub height: u32,
}

#[cfg(test)]
mod fee_sufficient_tests {
    use super::TrampolineRoutingPolicy;

    macro_rules! fee_sufficient_tests {
        ($($name:ident: $value:expr,)*) => {
        $(
            #[test]
            fn $name() {
                let (fee_base_msat, fee_proportional_millionths, sender_msat, invoice_msat, expected) = $value;
                let policy = TrampolineRoutingPolicy {
                    cltv_expiry_delta: 144,
                    fee_base_msat,
                    fee_proportional_millionths,
                };

                let sufficient = policy.fee_sufficient(sender_msat, invoice_msat);
                assert_eq!(expected, sufficient);
            }
        )*
        }
    }

    fee_sufficient_tests! {
        fee_5000ppm_success: (0, 5000, 1_005_000, 1_000_000, true),
        fee_5000ppm_underpaid: (0, 5000, 1_004_999, 1_000_000, false),
        fee_5000ppm_overpaid: (0, 5000, 1_005_001, 1_000_000, true),
        fee_1000base_success: (1000, 0, 1_001_000, 1_000_000, true),
        fee_1000base_underpaid: (1000, 0, 1_000_999, 1_000_000, false),
        fee_1000base_overpaid: (1000, 0, 1_001_001, 1_000_000, true),
        fee_1000base_5000ppm_success: (1000, 5000, 1_006_000, 1_000_000, true),
        fee_1000base_5000ppm_underpaid: (1000, 5000, 1_005_999, 1_000_000, false),
        fee_1000base_5000ppm_overpaid: (1000, 5000, 1_006_001, 1_000_000, true),
        fee_ppm_rounding: (0, 1, 999_999, 999_999, true),
        fee_ppm_rounding_boundary: (0, 1, 1_000_000, 1_000_000, false),
        fee_mul_overflow: (0, 2, u64::MAX, u64::MAX / 2 + 1, false),
        fee_mul_overflow_boundary: (0, 2, u64::MAX, u64::MAX / 2, true),
    }
}

#[cfg(test)]
mod encode_failure_tests {
    use crate::messages::TrampolineRoutingPolicy;

    use super::HtlcFailReason;

    #[test]
    fn encode_temporary_node_failure() {
        let failure = HtlcFailReason::TemporaryNodeFailure;
        let encoded = failure.encode();
        assert_eq!(vec![0x20, 2], encoded);
    }

    #[test]
    fn encode_temporary_trampoline_failure() {
        let failure = HtlcFailReason::TemporaryTrampolineFailure(TrampolineRoutingPolicy {
            fee_base_msat: 1,
            fee_proportional_millionths: 2,
            cltv_expiry_delta: 3,
        });
        let encoded = failure.encode();
        assert_eq!(vec![0x20, 26, 0, 0, 0, 1, 0, 0, 0, 2, 0, 3], encoded);
    }
}

#[cfg(test)]
mod serialize_cln_messages_tests {
    use crate::{
        messages::{Htlc, HtlcAcceptedResponse, Onion},
        tlv::SerializedTlvStream,
    };

    use super::HtlcAcceptedRequest;

    #[test]
    fn deserialize_htlc_accepted_request() {
        let raw = r#"{
            "onion": {
              "payload": "",
              "short_channel_id": "1x2x3",
              "forward_msat": 42,
              "outgoing_cltv_value": 500014,
              "shared_secret": "0000000000000000000000000000000000000000000000000000000000000000",
              "next_onion": ""
            },
            "htlc": {
              "short_channel_id": "4x5x6",
              "id": 27,
              "amount_msat": 43,
              "cltv_expiry": 500028,
              "cltv_expiry_relative": 10,
              "payment_hash": "0000000000000000000000000000000000000000000000000000000000000000"
            },
            "forward_to": "0000000000000000000000000000000000000000000000000000000000000000"
          }"#;

        let request: HtlcAcceptedRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(
            HtlcAcceptedRequest {
                htlc: Htlc {
                    short_channel_id: "4x5x6".parse().unwrap(),
                    id: 27,
                    amount_msat: 43,
                    cltv_expiry: 500028,
                    cltv_expiry_relative: 10,
                    payment_hash: [0; 32].to_vec()
                },
                onion: Onion {
                    payload: SerializedTlvStream::from(vec![]),
                    short_channel_id: Some("1x2x3".parse().unwrap()),
                    forward_msat: 42,
                    outgoing_cltv_value: 500014,
                    total_msat: None,
                },
            },
            request
        )
    }

    #[test]
    fn serialize_htlc_accepted_response_continue() {
        let resp = HtlcAcceptedResponse::Continue { payload: None };
        let j = serde_json::to_string(&resp).unwrap();
        assert_eq!(r#"{"result":"continue"}"#, j);
    }

    #[test]
    fn serialize_htlc_accepted_response_continue_with_payload() {
        let resp = HtlcAcceptedResponse::Continue {
            payload: Some(vec![1]),
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert_eq!(r#"{"result":"continue","payload":"01"}"#, j);
    }

    #[test]
    fn serialize_htlc_accepted_resolve() {
        let resp = HtlcAcceptedResponse::Resolve {
            payment_key: vec![1],
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert_eq!(r#"{"result":"resolve","payment_key":"01"}"#, j);
    }

    #[test]
    fn serialize_htlc_accepted_response_fail() {
        let resp = HtlcAcceptedResponse::Fail {
            failure_message: vec![1],
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert_eq!(r#"{"result":"fail","failure_message":"01"}"#, j);
    }
}
