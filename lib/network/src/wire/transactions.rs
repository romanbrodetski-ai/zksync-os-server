use alloy::primitives::Bytes;
use alloy::primitives::bytes::BufMut;
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};

/// Request to submit a raw L2 transaction to a consensus peer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, RlpEncodable, RlpDecodable)]
pub struct ForwardRawTransaction {
    pub request_id: u64,
    pub tx: Bytes,
}

/// Response for [`ForwardRawTransaction`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForwardRawTransactionResult {
    pub request_id: u64,
    pub error: Option<String>,
}

impl ForwardRawTransactionResult {
    pub fn accepted(request_id: u64) -> Self {
        Self {
            request_id,
            error: None,
        }
    }

    pub fn rejected(request_id: u64, error: String) -> Self {
        Self {
            request_id,
            error: Some(error),
        }
    }
}

impl Encodable for ForwardRawTransactionResult {
    fn encode(&self, out: &mut dyn BufMut) {
        self.request_id.encode(out);
        match &self.error {
            None => 0u8.encode(out),
            Some(error) => {
                1u8.encode(out);
                error.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        self.request_id.length()
            + 1u8.length()
            + self.error.as_ref().map_or(0, |error| error.length())
    }
}

impl Decodable for ForwardRawTransactionResult {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let request_id = u64::decode(buf)?;
        let tag = u8::decode(buf)?;
        let error = match tag {
            0 => None,
            1 => Some(String::decode(buf)?),
            _ => {
                return Err(alloy_rlp::Error::Custom(
                    "invalid forward raw transaction result tag",
                ));
            }
        };
        Ok(Self { request_id, error })
    }
}
