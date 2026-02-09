use alethia_reth_db::model::StoredL1Origin;
use alloy_primitives::{
    B256, U256,
    bytes::{Buf, BufMut},
};
use alloy_rlp::{BytesMut, Decodable, Encodable, RlpDecodable, RlpEncodable};
use reth_eth_wire::{Capability, message::RequestPair, protocol::Protocol};

/// Message type exchanged during `taiko_aux` protocol handshake.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaikoAuxNodeType {
    /// Full node serving auxiliary Taiko database tables.
    Full = 0x00,
}

impl Encodable for TaikoAuxNodeType {
    /// Encodes the node type as a single-byte discriminator.
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(*self as u8);
    }

    /// Returns the encoded byte length for node type messages.
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for TaikoAuxNodeType {
    /// Decodes a node type from a single-byte discriminator.
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let value = match buf.first().ok_or(alloy_rlp::Error::InputTooShort)? {
            0x00 => Self::Full,
            _ => return Err(alloy_rlp::Error::Custom("invalid taiko aux node type")),
        };
        buf.advance(1);
        Ok(value)
    }
}

/// Request range for auxiliary table rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct AuxRangeRequest {
    /// Inclusive start key.
    pub start: u64,
    /// Maximum number of rows.
    pub limit: u64,
}

/// L1 origin row transferred over p2p.
#[derive(Clone, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct AuxL1OriginEntry {
    /// L2 block number that owns this origin mapping.
    pub block_number: u64,
    /// Taiko block identifier stored in `StoredL1Origin`.
    pub block_id: U256,
    /// Hash of the L2 block.
    pub l2_block_hash: B256,
    /// L1 block height referenced by this L2 block.
    pub l1_block_height: U256,
    /// Hash of the referenced L1 block.
    pub l1_block_hash: B256,
    /// Build payload args identifier associated with this origin.
    pub build_payload_args_id: [u8; 8],
    /// Whether this block came from forced inclusion.
    pub is_forced_inclusion: bool,
    /// Signature bytes captured for this origin entry.
    pub signature: [u8; 65],
}

impl AuxL1OriginEntry {
    /// Converts an entry from table row representation.
    pub fn from_table_row(block_number: u64, value: StoredL1Origin) -> Self {
        Self {
            block_number,
            block_id: value.block_id,
            l2_block_hash: value.l2_block_hash,
            l1_block_height: value.l1_block_height,
            l1_block_hash: value.l1_block_hash,
            build_payload_args_id: value.build_payload_args_id,
            is_forced_inclusion: value.is_forced_inclusion,
            signature: value.signature,
        }
    }

    /// Converts an entry to table row representation.
    pub fn into_table_row(self) -> (u64, StoredL1Origin) {
        (
            self.block_number,
            StoredL1Origin {
                block_id: self.block_id,
                l2_block_hash: self.l2_block_hash,
                l1_block_height: self.l1_block_height,
                l1_block_hash: self.l1_block_hash,
                build_payload_args_id: self.build_payload_args_id,
                is_forced_inclusion: self.is_forced_inclusion,
                signature: self.signature,
            },
        )
    }
}

/// Batch -> last block row transferred over p2p.
#[derive(Clone, Copy, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct AuxBatchLastBlockEntry {
    /// Batch identifier.
    pub batch_id: u64,
    /// Last block number included in this batch.
    pub block_number: u64,
}

/// RLP-friendly optional block number wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct AuxOptionalBlockNumber {
    /// Indicates whether `value` is present.
    pub has_value: bool,
    /// Block number payload when `has_value` is true.
    pub value: u64,
}

impl AuxOptionalBlockNumber {
    /// Converts the wire representation into a standard `Option<u64>`.
    pub const fn into_option(self) -> Option<u64> {
        if self.has_value { Some(self.value) } else { None }
    }
}

impl From<Option<u64>> for AuxOptionalBlockNumber {
    /// Converts an optional block number into the wire-friendly wrapper.
    fn from(value: Option<u64>) -> Self {
        match value {
            Some(value) => Self { has_value: true, value },
            None => Self { has_value: false, value: 0 },
        }
    }
}

/// Message IDs for the `taiko_aux` protocol.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaikoAuxMessageId {
    /// Announces peer node type.
    NodeType = 0x00,
    /// Requests a range of L1 origin rows.
    GetL1Origins = 0x01,
    /// Returns L1 origin rows for a previous request.
    L1Origins = 0x02,
    /// Requests the head L1 origin pointer.
    GetHeadL1Origin = 0x03,
    /// Returns the head L1 origin pointer.
    HeadL1Origin = 0x04,
    /// Requests a range of batch->last-block rows.
    GetBatchLastBlocks = 0x05,
    /// Returns batch->last-block rows for a previous request.
    BatchLastBlocks = 0x06,
}

impl Encodable for TaikoAuxMessageId {
    /// Encodes the message ID as a single-byte discriminator.
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(*self as u8);
    }

    /// Returns the encoded byte length for message IDs.
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for TaikoAuxMessageId {
    /// Decodes a message ID from a single-byte discriminator.
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let id = match buf.first().ok_or(alloy_rlp::Error::InputTooShort)? {
            0x00 => Self::NodeType,
            0x01 => Self::GetL1Origins,
            0x02 => Self::L1Origins,
            0x03 => Self::GetHeadL1Origin,
            0x04 => Self::HeadL1Origin,
            0x05 => Self::GetBatchLastBlocks,
            0x06 => Self::BatchLastBlocks,
            _ => return Err(alloy_rlp::Error::Custom("invalid taiko aux message id")),
        };
        buf.advance(1);
        Ok(id)
    }
}

/// A message in the `taiko_aux` protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaikoAuxMessage {
    /// Node type advertisement.
    NodeType(TaikoAuxNodeType),
    /// L1 origin range request.
    GetL1Origins(RequestPair<AuxRangeRequest>),
    /// L1 origin range response.
    L1Origins(RequestPair<Vec<AuxL1OriginEntry>>),
    /// Head origin request.
    GetHeadL1Origin(RequestPair<u8>),
    /// Head origin response.
    HeadL1Origin(RequestPair<AuxOptionalBlockNumber>),
    /// Batch rows range request.
    GetBatchLastBlocks(RequestPair<AuxRangeRequest>),
    /// Batch rows range response.
    BatchLastBlocks(RequestPair<Vec<AuxBatchLastBlockEntry>>),
}

impl TaikoAuxMessage {
    /// Returns the message ID corresponding to this message variant.
    pub const fn message_id(&self) -> TaikoAuxMessageId {
        match self {
            Self::NodeType(_) => TaikoAuxMessageId::NodeType,
            Self::GetL1Origins(_) => TaikoAuxMessageId::GetL1Origins,
            Self::L1Origins(_) => TaikoAuxMessageId::L1Origins,
            Self::GetHeadL1Origin(_) => TaikoAuxMessageId::GetHeadL1Origin,
            Self::HeadL1Origin(_) => TaikoAuxMessageId::HeadL1Origin,
            Self::GetBatchLastBlocks(_) => TaikoAuxMessageId::GetBatchLastBlocks,
            Self::BatchLastBlocks(_) => TaikoAuxMessageId::BatchLastBlocks,
        }
    }

    /// Converts this message into a protocol envelope.
    pub const fn into_protocol_message(self) -> TaikoAuxProtocolMessage {
        let message_type = self.message_id();
        TaikoAuxProtocolMessage { message_type, message: self }
    }
}

impl From<TaikoAuxMessage> for TaikoAuxProtocolMessage {
    /// Wraps a protocol message variant in its envelope.
    fn from(value: TaikoAuxMessage) -> Self {
        value.into_protocol_message()
    }
}

impl Encodable for TaikoAuxMessage {
    /// Encodes the inner payload for the selected message variant.
    fn encode(&self, out: &mut dyn BufMut) {
        match self {
            Self::NodeType(node_type) => node_type.encode(out),
            Self::GetL1Origins(request) => request.encode(out),
            Self::L1Origins(response) => response.encode(out),
            Self::GetHeadL1Origin(request) => request.encode(out),
            Self::HeadL1Origin(response) => response.encode(out),
            Self::GetBatchLastBlocks(request) => request.encode(out),
            Self::BatchLastBlocks(response) => response.encode(out),
        }
    }

    /// Returns encoded length of the inner message payload.
    fn length(&self) -> usize {
        match self {
            Self::NodeType(node_type) => node_type.length(),
            Self::GetL1Origins(request) => request.length(),
            Self::L1Origins(response) => response.length(),
            Self::GetHeadL1Origin(request) => request.length(),
            Self::HeadL1Origin(response) => response.length(),
            Self::GetBatchLastBlocks(request) => request.length(),
            Self::BatchLastBlocks(response) => response.length(),
        }
    }
}

/// Protocol message envelope for `taiko_aux`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaikoAuxProtocolMessage {
    /// Message discriminator used to decode the payload.
    pub message_type: TaikoAuxMessageId,
    /// Typed protocol payload.
    pub message: TaikoAuxMessage,
}

impl TaikoAuxProtocolMessage {
    /// Returns protocol capability descriptor.
    pub const fn capability() -> Capability {
        Capability::new_static("taiko_aux", 1)
    }

    /// Returns protocol metadata.
    pub const fn protocol() -> Protocol {
        Protocol::new(Self::capability(), 7)
    }

    /// Creates node type message.
    pub const fn node_type(node_type: TaikoAuxNodeType) -> Self {
        TaikoAuxMessage::NodeType(node_type).into_protocol_message()
    }

    /// Creates l1 origin request.
    pub const fn get_l1_origins(request_id: u64, request: AuxRangeRequest) -> Self {
        TaikoAuxMessage::GetL1Origins(RequestPair { request_id, message: request })
            .into_protocol_message()
    }

    /// Creates l1 origin response.
    pub const fn l1_origins(request_id: u64, rows: Vec<AuxL1OriginEntry>) -> Self {
        TaikoAuxMessage::L1Origins(RequestPair { request_id, message: rows })
            .into_protocol_message()
    }

    /// Creates head origin request.
    pub const fn get_head_l1_origin(request_id: u64) -> Self {
        TaikoAuxMessage::GetHeadL1Origin(RequestPair { request_id, message: 0u8 })
            .into_protocol_message()
    }

    /// Creates head origin response.
    pub fn head_l1_origin(request_id: u64, head: Option<u64>) -> Self {
        TaikoAuxMessage::HeadL1Origin(RequestPair {
            request_id,
            message: AuxOptionalBlockNumber::from(head),
        })
        .into_protocol_message()
    }

    /// Creates batch->last block request.
    pub const fn get_batch_last_blocks(request_id: u64, request: AuxRangeRequest) -> Self {
        TaikoAuxMessage::GetBatchLastBlocks(RequestPair { request_id, message: request })
            .into_protocol_message()
    }

    /// Creates batch->last block response.
    pub const fn batch_last_blocks(request_id: u64, rows: Vec<AuxBatchLastBlockEntry>) -> Self {
        TaikoAuxMessage::BatchLastBlocks(RequestPair { request_id, message: rows })
            .into_protocol_message()
    }

    /// Returns RLPx wire bytes.
    pub fn encoded(&self) -> BytesMut {
        let mut out = BytesMut::with_capacity(self.length());
        self.encode(&mut out);
        out
    }

    /// Decodes a message from wire bytes.
    pub fn decode_message(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let message_type = TaikoAuxMessageId::decode(buf)?;
        let message = match message_type {
            TaikoAuxMessageId::NodeType => {
                TaikoAuxMessage::NodeType(TaikoAuxNodeType::decode(buf)?)
            }
            TaikoAuxMessageId::GetL1Origins => {
                TaikoAuxMessage::GetL1Origins(RequestPair::decode(buf)?)
            }
            TaikoAuxMessageId::L1Origins => TaikoAuxMessage::L1Origins(RequestPair::decode(buf)?),
            TaikoAuxMessageId::GetHeadL1Origin => {
                TaikoAuxMessage::GetHeadL1Origin(RequestPair::decode(buf)?)
            }
            TaikoAuxMessageId::HeadL1Origin => {
                TaikoAuxMessage::HeadL1Origin(RequestPair::decode(buf)?)
            }
            TaikoAuxMessageId::GetBatchLastBlocks => {
                TaikoAuxMessage::GetBatchLastBlocks(RequestPair::decode(buf)?)
            }
            TaikoAuxMessageId::BatchLastBlocks => {
                TaikoAuxMessage::BatchLastBlocks(RequestPair::decode(buf)?)
            }
        };
        Ok(Self { message_type, message })
    }
}

impl Encodable for TaikoAuxProtocolMessage {
    /// Encodes the envelope as `message_type || message_payload`.
    fn encode(&self, out: &mut dyn BufMut) {
        self.message_type.encode(out);
        self.message.encode(out);
    }

    /// Returns total encoded size of the envelope and payload.
    fn length(&self) -> usize {
        self.message_type.length() + self.message.length()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_message_count() {
        assert_eq!(TaikoAuxProtocolMessage::protocol().messages(), 7);
    }

    #[test]
    fn l1_origins_roundtrip() {
        let entry = AuxL1OriginEntry {
            block_number: 1,
            block_id: U256::from(1u64),
            l2_block_hash: B256::from([1u8; 32]),
            l1_block_height: U256::from(100u64),
            l1_block_hash: B256::from([2u8; 32]),
            build_payload_args_id: [3u8; 8],
            is_forced_inclusion: true,
            signature: [4u8; 65],
        };
        let msg = TaikoAuxProtocolMessage {
            message_type: TaikoAuxMessageId::L1Origins,
            message: TaikoAuxMessage::L1Origins(RequestPair {
                request_id: 7,
                message: vec![entry],
            }),
        };

        let encoded = msg.encoded();
        let decoded = TaikoAuxProtocolMessage::decode_message(&mut &encoded[..]).unwrap();

        assert_eq!(decoded, msg);
    }

    #[test]
    fn batch_rows_roundtrip() {
        let msg = TaikoAuxProtocolMessage {
            message_type: TaikoAuxMessageId::BatchLastBlocks,
            message: TaikoAuxMessage::BatchLastBlocks(RequestPair {
                request_id: 11,
                message: vec![AuxBatchLastBlockEntry { batch_id: 3, block_number: 99 }],
            }),
        };

        let encoded = msg.encoded();
        let decoded = TaikoAuxProtocolMessage::decode_message(&mut &encoded[..]).unwrap();

        assert_eq!(decoded, msg);
    }

    #[test]
    fn head_l1_origin_roundtrip() {
        let msg = TaikoAuxProtocolMessage::head_l1_origin(5, Some(42));
        let encoded = msg.encoded();
        let decoded = TaikoAuxProtocolMessage::decode_message(&mut &encoded[..]).unwrap();
        assert_eq!(decoded, msg);
    }
}
