#![allow(non_camel_case_types)]
#![allow(dead_code)]
#![allow(unreachable_code)]

#[allow(unused_imports)]
use binschema_runtime::{
    BinSchemaError, BitOrder, BitStreamDecoder, BitStreamEncoder, EncodeContext, Endianness,
    FieldValue, Result,
};
#[allow(unused_imports)]
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum WheatProtocolMessage {
    ServerHello(ServerHelloOutput),
    ClientHello(ClientHelloOutput),
    SessionEstablished(SessionEstablishedOutput),
    RepositoryOpen(RepositoryOpenOutput),
    RepositoryState(RepositoryStateOutput),
    ListLabels(ListLabelsOutput),
    ListLabelsResult(ListLabelsResultOutput),
    FetchRequest(FetchRequestOutput),
    ObjectBatch(ObjectBatchOutput),
    ObjectWant(ObjectWantOutput),
    FetchComplete(FetchCompleteOutput),
    ContentFetchRequest(ContentFetchRequestOutput),
    ContentBatch(ContentBatchOutput),
    ContentChunk(ContentChunkOutput),
    ContentRedirect(ContentRedirectOutput),
    PublishRequest(PublishRequestOutput),
    ObjectsNeeded(ObjectsNeededOutput),
    ObjectsComplete(ObjectsCompleteOutput),
    ContentNeeded(ContentNeededOutput),
    ContentUpload(ContentUploadOutput),
    ContentUploadChunk(ContentUploadChunkOutput),
    ContentUploadComplete(ContentUploadCompleteOutput),
    PublishCommit(PublishCommitOutput),
    PublishProgress(PublishProgressOutput),
    PublishResult(PublishResultOutput),
    Credit(CreditOutput),
    Error(ErrorOutput),
}

impl WheatProtocolMessage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            WheatProtocolMessage::ServerHello(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint16(v.protocol_version_min, Endianness::BigEndian);
                encoder.write_uint16(v.protocol_version_max, Endianness::BigEndian);
                for item in &v.server_id {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint16(v.capabilities.len() as u16, Endianness::BigEndian);
                for item in &v.capabilities {
                    encoder.write_uint16(item.len() as u16, Endianness::BigEndian);
                    for b in item.as_bytes() {
                        encoder.write_uint8(*b);
                    }
                }
            }
            WheatProtocolMessage::ClientHello(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint16(v.selected_version, Endianness::BigEndian);
                encoder.write_uint16(v.auth_token.len() as u16, Endianness::BigEndian);
                for item in &v.auth_token {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint32(v.receive_window_bytes, Endianness::BigEndian);
                encoder.write_uint16(v.receive_window_messages, Endianness::BigEndian);
            }
            WheatProtocolMessage::SessionEstablished(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint16(v.selected_version, Endianness::BigEndian);
                encoder.write_uint32(v.receive_window_bytes, Endianness::BigEndian);
                encoder.write_uint16(v.receive_window_messages, Endianness::BigEndian);
                v.status.encode_into(encoder)?;
                encoder.write_uint16(v.supported_range_min, Endianness::BigEndian);
                encoder.write_uint16(v.supported_range_max, Endianness::BigEndian);
                encoder.write_uint16(v.message.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.message.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
            }
            WheatProtocolMessage::RepositoryOpen(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint16(v.name.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.name.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
            }
            WheatProtocolMessage::RepositoryState(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint64(v.head_generation, Endianness::BigEndian);
                v.head_envelope.encode_into(encoder)?;
                encoder.write_uint16(v.labels.len() as u16, Endianness::BigEndian);
                for item in &v.labels {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint8(if v.labels_truncated { 1 } else { 0 });
            }
            WheatProtocolMessage::ListLabels(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.cursor.len() as u16, Endianness::BigEndian);
                for item in &v.cursor {
                    encoder.write_uint8(*item);
                }
            }
            WheatProtocolMessage::ListLabelsResult(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.labels.len() as u16, Endianness::BigEndian);
                for item in &v.labels {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint16(v.next_cursor.len() as u16, Endianness::BigEndian);
                for item in &v.next_cursor {
                    encoder.write_uint8(*item);
                }
            }
            WheatProtocolMessage::FetchRequest(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.labels.len() as u16, Endianness::BigEndian);
                for item in &v.labels {
                    encoder.write_uint16(item.len() as u16, Endianness::BigEndian);
                    for b in item.as_bytes() {
                        encoder.write_uint8(*b);
                    }
                }
                encoder.write_uint64(v.since_generation, Endianness::BigEndian);
                v.since_envelope.encode_into(encoder)?;
            }
            WheatProtocolMessage::ObjectBatch(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint32(v.sequence, Endianness::BigEndian);
                encoder.write_uint16(v.objects.len() as u16, Endianness::BigEndian);
                for item in &v.objects {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::ObjectWant(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.ids.len() as u16, Endianness::BigEndian);
                for item in &v.ids {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::FetchComplete(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
            }
            WheatProtocolMessage::ContentFetchRequest(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.content_ids.len() as u16, Endianness::BigEndian);
                for item in &v.content_ids {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::ContentBatch(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint32(v.sequence, Endianness::BigEndian);
                encoder.write_uint16(v.contents.len() as u16, Endianness::BigEndian);
                for item in &v.contents {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::ContentChunk(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.content_id.encode_into(encoder)?;
                encoder.write_uint64(v.offset, Endianness::BigEndian);
                encoder.write_uint32(v.data.len() as u32, Endianness::BigEndian);
                for item in &v.data {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint8(if v.is_final { 1 } else { 0 });
            }
            WheatProtocolMessage::ContentRedirect(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.content_id.encode_into(encoder)?;
                encoder.write_uint16(v.url.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.url.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
                encoder.write_uint32(v.expires_seconds, Endianness::BigEndian);
            }
            WheatProtocolMessage::PublishRequest(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.publish_key.encode_into(encoder)?;
                v.envelope_id.encode_into(encoder)?;
                encoder.write_uint64(v.expected_generation, Endianness::BigEndian);
                v.expected_envelope.encode_into(encoder)?;
                encoder.write_uint8(if v.force { 1 } else { 0 });
            }
            WheatProtocolMessage::ObjectsNeeded(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.ids.len() as u16, Endianness::BigEndian);
                for item in &v.ids {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::ObjectsComplete(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
            }
            WheatProtocolMessage::ContentNeeded(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.content_ids.len() as u16, Endianness::BigEndian);
                for item in &v.content_ids {
                    item.encode_into(encoder)?;
                }
            }
            WheatProtocolMessage::ContentUpload(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.content_id.encode_into(encoder)?;
                encoder.write_uint32(v.data.len() as u32, Endianness::BigEndian);
                for item in &v.data {
                    encoder.write_uint8(*item);
                }
            }
            WheatProtocolMessage::ContentUploadChunk(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.content_id.encode_into(encoder)?;
                encoder.write_uint64(v.offset, Endianness::BigEndian);
                encoder.write_uint32(v.data.len() as u32, Endianness::BigEndian);
                for item in &v.data {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint8(if v.is_final { 1 } else { 0 });
            }
            WheatProtocolMessage::ContentUploadComplete(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
            }
            WheatProtocolMessage::PublishCommit(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
            }
            WheatProtocolMessage::PublishProgress(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.phase.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.phase.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
                encoder.write_uint8(v.percent);
            }
            WheatProtocolMessage::PublishResult(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                v.receipt_id.encode_into(encoder)?;
                v.status.encode_into(encoder)?;
                encoder.write_uint64(v.new_generation, Endianness::BigEndian);
                v.new_envelope.encode_into(encoder)?;
                encoder.write_uint16(v.message.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.message.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
            }
            WheatProtocolMessage::Credit(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint32(v.ack_through_sequence, Endianness::BigEndian);
                encoder.write_uint32(v.window_bytes, Endianness::BigEndian);
                encoder.write_uint16(v.window_messages, Endianness::BigEndian);
            }
            WheatProtocolMessage::Error(v) => {
                encoder.write_uint8(v.message_type);
                encoder.write_uint32(v.op_id, Endianness::BigEndian);
                encoder.write_uint16(v.code, Endianness::BigEndian);
                v.scope.encode_into(encoder)?;
                encoder.write_uint8(if v.retryable { 1 } else { 0 });
                encoder.write_uint16(v.message.len() as u16, Endianness::BigEndian);
                let string_bytes: &[u8] = v.message.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
                v.object_id.encode_into(encoder)?;
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.peek_uint8()?;
        // Match on discriminator value
        if value == 0x01 {
            Ok(WheatProtocolMessage::ServerHello(
                ServerHelloOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x02 {
            Ok(WheatProtocolMessage::ClientHello(
                ClientHelloOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x03 {
            Ok(WheatProtocolMessage::SessionEstablished(
                SessionEstablishedOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x10 {
            Ok(WheatProtocolMessage::RepositoryOpen(
                RepositoryOpenOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x11 {
            Ok(WheatProtocolMessage::RepositoryState(
                RepositoryStateOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x12 {
            Ok(WheatProtocolMessage::ListLabels(
                ListLabelsOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x13 {
            Ok(WheatProtocolMessage::ListLabelsResult(
                ListLabelsResultOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x20 {
            Ok(WheatProtocolMessage::FetchRequest(
                FetchRequestOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x21 {
            Ok(WheatProtocolMessage::ObjectBatch(
                ObjectBatchOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x22 {
            Ok(WheatProtocolMessage::ObjectWant(
                ObjectWantOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x23 {
            Ok(WheatProtocolMessage::FetchComplete(
                FetchCompleteOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x30 {
            Ok(WheatProtocolMessage::ContentFetchRequest(
                ContentFetchRequestOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x31 {
            Ok(WheatProtocolMessage::ContentBatch(
                ContentBatchOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x32 {
            Ok(WheatProtocolMessage::ContentChunk(
                ContentChunkOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x33 {
            Ok(WheatProtocolMessage::ContentRedirect(
                ContentRedirectOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x40 {
            Ok(WheatProtocolMessage::PublishRequest(
                PublishRequestOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x41 {
            Ok(WheatProtocolMessage::ObjectsNeeded(
                ObjectsNeededOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x42 {
            Ok(WheatProtocolMessage::ObjectsComplete(
                ObjectsCompleteOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x43 {
            Ok(WheatProtocolMessage::ContentNeeded(
                ContentNeededOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x44 {
            Ok(WheatProtocolMessage::ContentUpload(
                ContentUploadOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x45 {
            Ok(WheatProtocolMessage::ContentUploadChunk(
                ContentUploadChunkOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x46 {
            Ok(WheatProtocolMessage::ContentUploadComplete(
                ContentUploadCompleteOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x47 {
            Ok(WheatProtocolMessage::PublishCommit(
                PublishCommitOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x48 {
            Ok(WheatProtocolMessage::PublishProgress(
                PublishProgressOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x49 {
            Ok(WheatProtocolMessage::PublishResult(
                PublishResultOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x60 {
            Ok(WheatProtocolMessage::Credit(
                CreditOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x70 {
            Ok(WheatProtocolMessage::Error(
                ErrorOutput::decode_with_decoder(decoder)?,
            ))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "unknown discriminator value: {}",
                value
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Digest256 {
    pub bytes: Vec<u8>,
}

impl Digest256 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        for item in &self.bytes {
            encoder.write_byte(*item);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let bytes = decoder.read_bytes_vec(32)?;
        Ok(Self { bytes })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Uuid128 {
    pub bytes: Vec<u8>,
}

impl Uuid128 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        for item in &self.bytes {
            encoder.write_byte(*item);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let bytes = decoder.read_bytes_vec(16)?;
        Ok(Self { bytes })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LabelEntry {
    pub name: std::string::String,
    pub view_id: Digest256,
}

impl LabelEntry {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(self.name.len() as u16);
        let string_bytes: &[u8] = self.name.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.view_id.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let name = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let view_id = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { name, view_id })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectEntry {
    pub id: Digest256,
    pub data: Vec<u8>,
}

impl ObjectEntry {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.id.encode_into(encoder)?;
        encoder.write_u32_be(self.data.len() as u32);
        for item in &self.data {
            encoder.write_byte(*item);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let id = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let data = decoder.read_bytes_vec(length)?;
        Ok(Self { id, data })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentEntry {
    pub id: Digest256,
    pub data: Vec<u8>,
}

impl ContentEntry {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.id.encode_into(encoder)?;
        encoder.write_u32_be(self.data.len() as u32);
        for item in &self.data {
            encoder.write_byte(*item);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let id = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let data = decoder.read_bytes_vec(length)?;
        Ok(Self { id, data })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerHelloInput {
    pub protocol_version_min: u16,
    pub protocol_version_max: u16,
    pub server_id: Vec<u8>,
    pub capabilities: Vec<std::string::String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerHelloOutput {
    pub message_type: u8,
    pub protocol_version_min: u16,
    pub protocol_version_max: u16,
    pub server_id: Vec<u8>,
    pub capabilities: Vec<std::string::String>,
}

pub type ServerHello = ServerHelloOutput;

impl ServerHelloInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        encoder.write_u16_be(self.protocol_version_min);
        encoder.write_u16_be(self.protocol_version_max);
        for item in &self.server_id {
            encoder.write_byte(*item);
        }
        encoder.write_u16_be(self.capabilities.len() as u16);
        for item in &self.capabilities {
            encoder.write_u16_be(item.len() as u16);
            for b in item.as_bytes() {
                encoder.write_byte(*b);
            }
        }
        Ok(())
    }
}

impl ServerHelloOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                message_type
            )));
        }
        let protocol_version_min = decoder.read_u16_be()?;
        let protocol_version_max = decoder.read_u16_be()?;
        let server_id = decoder.read_bytes_vec(32)?;
        let length = decoder.read_u16_be()? as usize;
        let mut capabilities = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let str_len = decoder.read_u16_be()? as usize;
            let str_bytes = decoder.read_bytes_vec(str_len)?;
            let item = std::string::String::from_utf8(str_bytes)
                .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
            capabilities.push(item);
        }
        Ok(Self {
            message_type,
            protocol_version_min,
            protocol_version_max,
            server_id,
            capabilities,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ServerHelloInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ServerHelloInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ServerHelloOutput> for ServerHelloInput {
    fn from(o: ServerHelloOutput) -> Self {
        Self {
            protocol_version_min: o.protocol_version_min,
            protocol_version_max: o.protocol_version_max,
            server_id: o.server_id,
            capabilities: o.capabilities,
        }
    }
}

impl From<ServerHelloInput> for ServerHelloOutput {
    fn from(i: ServerHelloInput) -> Self {
        Self {
            message_type: 1u8,
            protocol_version_min: i.protocol_version_min,
            protocol_version_max: i.protocol_version_max,
            server_id: i.server_id,
            capabilities: i.capabilities,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientHelloInput {
    pub selected_version: u16,
    pub auth_token: Vec<u8>,
    pub receive_window_bytes: u32,
    pub receive_window_messages: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientHelloOutput {
    pub message_type: u8,
    pub selected_version: u16,
    pub auth_token: Vec<u8>,
    pub receive_window_bytes: u32,
    pub receive_window_messages: u16,
}

pub type ClientHello = ClientHelloOutput;

impl ClientHelloInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(2);
        encoder.write_u16_be(self.selected_version);
        encoder.write_u16_be(self.auth_token.len() as u16);
        for item in &self.auth_token {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.receive_window_bytes);
        encoder.write_u16_be(self.receive_window_messages);
        Ok(())
    }
}

impl ClientHelloOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 2u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2, got {}",
                message_type
            )));
        }
        let selected_version = decoder.read_u16_be()?;
        let length = decoder.read_u16_be()? as usize;
        let auth_token = decoder.read_bytes_vec(length)?;
        let receive_window_bytes = decoder.read_u32_be()?;
        let receive_window_messages = decoder.read_u16_be()?;
        Ok(Self {
            message_type,
            selected_version,
            auth_token,
            receive_window_bytes,
            receive_window_messages,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ClientHelloInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ClientHelloInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ClientHelloOutput> for ClientHelloInput {
    fn from(o: ClientHelloOutput) -> Self {
        Self {
            selected_version: o.selected_version,
            auth_token: o.auth_token,
            receive_window_bytes: o.receive_window_bytes,
            receive_window_messages: o.receive_window_messages,
        }
    }
}

impl From<ClientHelloInput> for ClientHelloOutput {
    fn from(i: ClientHelloInput) -> Self {
        Self {
            message_type: 2u8,
            selected_version: i.selected_version,
            auth_token: i.auth_token,
            receive_window_bytes: i.receive_window_bytes,
            receive_window_messages: i.receive_window_messages,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEstablishedStatus {
    SessionStatusOk(SessionStatusOkOutput),
    SessionStatusVersionMismatch(SessionStatusVersionMismatchOutput),
    SessionStatusAuthFailed(SessionStatusAuthFailedOutput),
}

impl SessionEstablishedStatus {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            SessionEstablishedStatus::SessionStatusOk(v) => {
                encoder.write_uint8(v.code);
            }
            SessionEstablishedStatus::SessionStatusVersionMismatch(v) => {
                encoder.write_uint8(v.code);
            }
            SessionEstablishedStatus::SessionStatusAuthFailed(v) => {
                encoder.write_uint8(v.code);
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.peek_uint8()?;
        // Match on discriminator value
        if value == 0 {
            Ok(SessionEstablishedStatus::SessionStatusOk(
                SessionStatusOkOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(SessionEstablishedStatus::SessionStatusVersionMismatch(
                SessionStatusVersionMismatchOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 2 {
            Ok(SessionEstablishedStatus::SessionStatusAuthFailed(
                SessionStatusAuthFailedOutput::decode_with_decoder(decoder)?,
            ))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "unknown discriminator value: {}",
                value
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusOkInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusOkOutput {
    pub code: u8,
}

pub type SessionStatusOk = SessionStatusOkOutput;

impl SessionStatusOkInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        Ok(())
    }
}

impl SessionStatusOkOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SessionStatusOkInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SessionStatusOkInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<SessionStatusOkOutput> for SessionStatusOkInput {
    fn from(_o: SessionStatusOkOutput) -> Self {
        Self {}
    }
}

impl From<SessionStatusOkInput> for SessionStatusOkOutput {
    fn from(_i: SessionStatusOkInput) -> Self {
        Self { code: 0u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusVersionMismatchInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusVersionMismatchOutput {
    pub code: u8,
}

pub type SessionStatusVersionMismatch = SessionStatusVersionMismatchOutput;

impl SessionStatusVersionMismatchInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        Ok(())
    }
}

impl SessionStatusVersionMismatchOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SessionStatusVersionMismatchInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SessionStatusVersionMismatchInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<SessionStatusVersionMismatchOutput> for SessionStatusVersionMismatchInput {
    fn from(_o: SessionStatusVersionMismatchOutput) -> Self {
        Self {}
    }
}

impl From<SessionStatusVersionMismatchInput> for SessionStatusVersionMismatchOutput {
    fn from(_i: SessionStatusVersionMismatchInput) -> Self {
        Self { code: 1u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusAuthFailedInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatusAuthFailedOutput {
    pub code: u8,
}

pub type SessionStatusAuthFailed = SessionStatusAuthFailedOutput;

impl SessionStatusAuthFailedInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(2);
        Ok(())
    }
}

impl SessionStatusAuthFailedOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 2u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SessionStatusAuthFailedInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SessionStatusAuthFailedInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<SessionStatusAuthFailedOutput> for SessionStatusAuthFailedInput {
    fn from(_o: SessionStatusAuthFailedOutput) -> Self {
        Self {}
    }
}

impl From<SessionStatusAuthFailedInput> for SessionStatusAuthFailedOutput {
    fn from(_i: SessionStatusAuthFailedInput) -> Self {
        Self { code: 2u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEstablishedInput {
    pub selected_version: u16,
    pub receive_window_bytes: u32,
    pub receive_window_messages: u16,
    pub status: SessionEstablishedStatus,
    pub supported_range_min: u16,
    pub supported_range_max: u16,
    pub message: std::string::String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEstablishedOutput {
    pub message_type: u8,
    pub selected_version: u16,
    pub receive_window_bytes: u32,
    pub receive_window_messages: u16,
    pub status: SessionEstablishedStatus,
    pub supported_range_min: u16,
    pub supported_range_max: u16,
    pub message: std::string::String,
}

pub type SessionEstablished = SessionEstablishedOutput;

impl SessionEstablishedInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(3);
        encoder.write_u16_be(self.selected_version);
        encoder.write_u32_be(self.receive_window_bytes);
        encoder.write_u16_be(self.receive_window_messages);
        self.status.encode_into(encoder)?;
        encoder.write_u16_be(self.supported_range_min);
        encoder.write_u16_be(self.supported_range_max);
        encoder.write_u16_be(self.message.len() as u16);
        let string_bytes: &[u8] = self.message.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        Ok(())
    }
}

impl SessionEstablishedOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 3u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3, got {}",
                message_type
            )));
        }
        let selected_version = decoder.read_u16_be()?;
        let receive_window_bytes = decoder.read_u32_be()?;
        let receive_window_messages = decoder.read_u16_be()?;
        let status = SessionEstablishedStatus::decode_with_decoder(decoder)?;
        let supported_range_min = decoder.read_u16_be()?;
        let supported_range_max = decoder.read_u16_be()?;
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let message = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        Ok(Self {
            message_type,
            selected_version,
            receive_window_bytes,
            receive_window_messages,
            status,
            supported_range_min,
            supported_range_max,
            message,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SessionEstablishedInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SessionEstablishedInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<SessionEstablishedOutput> for SessionEstablishedInput {
    fn from(o: SessionEstablishedOutput) -> Self {
        Self {
            selected_version: o.selected_version,
            receive_window_bytes: o.receive_window_bytes,
            receive_window_messages: o.receive_window_messages,
            status: o.status,
            supported_range_min: o.supported_range_min,
            supported_range_max: o.supported_range_max,
            message: o.message,
        }
    }
}

impl From<SessionEstablishedInput> for SessionEstablishedOutput {
    fn from(i: SessionEstablishedInput) -> Self {
        Self {
            message_type: 3u8,
            selected_version: i.selected_version,
            receive_window_bytes: i.receive_window_bytes,
            receive_window_messages: i.receive_window_messages,
            status: i.status,
            supported_range_min: i.supported_range_min,
            supported_range_max: i.supported_range_max,
            message: i.message,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryOpenInput {
    pub name: std::string::String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryOpenOutput {
    pub message_type: u8,
    pub name: std::string::String,
}

pub type RepositoryOpen = RepositoryOpenOutput;

impl RepositoryOpenInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(16);
        encoder.write_u16_be(self.name.len() as u16);
        let string_bytes: &[u8] = self.name.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        Ok(())
    }
}

impl RepositoryOpenOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 16u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 16, got {}",
                message_type
            )));
        }
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let name = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        Ok(Self { message_type, name })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RepositoryOpenInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RepositoryOpenInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<RepositoryOpenOutput> for RepositoryOpenInput {
    fn from(o: RepositoryOpenOutput) -> Self {
        Self { name: o.name }
    }
}

impl From<RepositoryOpenInput> for RepositoryOpenOutput {
    fn from(i: RepositoryOpenInput) -> Self {
        Self {
            message_type: 16u8,
            name: i.name,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateInput {
    pub head_generation: u64,
    pub head_envelope: Digest256,
    pub labels: Vec<LabelEntry>,
    pub labels_truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateOutput {
    pub message_type: u8,
    pub head_generation: u64,
    pub head_envelope: Digest256,
    pub labels: Vec<LabelEntry>,
    pub labels_truncated: bool,
}

pub type RepositoryState = RepositoryStateOutput;

impl RepositoryStateInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(17);
        encoder.write_u64_be(self.head_generation);
        self.head_envelope.encode_into(encoder)?;
        encoder.write_u16_be(self.labels.len() as u16);
        for item in &self.labels {
            item.encode_into(encoder)?;
        }
        encoder.write_byte(if self.labels_truncated { 1 } else { 0 });
        Ok(())
    }
}

impl RepositoryStateOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 17u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 17, got {}",
                message_type
            )));
        }
        let head_generation = decoder.read_u64_be()?;
        let head_envelope = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u16_be()? as usize;
        let mut labels = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = LabelEntry::decode_with_decoder(decoder)?;
            labels.push(item);
        }
        let labels_truncated = decoder.read_byte()? != 0;
        Ok(Self {
            message_type,
            head_generation,
            head_envelope,
            labels,
            labels_truncated,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RepositoryStateInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RepositoryStateInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<RepositoryStateOutput> for RepositoryStateInput {
    fn from(o: RepositoryStateOutput) -> Self {
        Self {
            head_generation: o.head_generation,
            head_envelope: o.head_envelope,
            labels: o.labels,
            labels_truncated: o.labels_truncated,
        }
    }
}

impl From<RepositoryStateInput> for RepositoryStateOutput {
    fn from(i: RepositoryStateInput) -> Self {
        Self {
            message_type: 17u8,
            head_generation: i.head_generation,
            head_envelope: i.head_envelope,
            labels: i.labels,
            labels_truncated: i.labels_truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListLabelsInput {
    pub op_id: u32,
    pub cursor: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListLabelsOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub cursor: Vec<u8>,
}

pub type ListLabels = ListLabelsOutput;

impl ListLabelsInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(18);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.cursor.len() as u16);
        for item in &self.cursor {
            encoder.write_byte(*item);
        }
        Ok(())
    }
}

impl ListLabelsOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 18u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 18, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let cursor = decoder.read_bytes_vec(length)?;
        Ok(Self {
            message_type,
            op_id,
            cursor,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ListLabelsInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ListLabelsInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ListLabelsOutput> for ListLabelsInput {
    fn from(o: ListLabelsOutput) -> Self {
        Self {
            op_id: o.op_id,
            cursor: o.cursor,
        }
    }
}

impl From<ListLabelsInput> for ListLabelsOutput {
    fn from(i: ListLabelsInput) -> Self {
        Self {
            message_type: 18u8,
            op_id: i.op_id,
            cursor: i.cursor,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListLabelsResultInput {
    pub op_id: u32,
    pub labels: Vec<LabelEntry>,
    pub next_cursor: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListLabelsResultOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub labels: Vec<LabelEntry>,
    pub next_cursor: Vec<u8>,
}

pub type ListLabelsResult = ListLabelsResultOutput;

impl ListLabelsResultInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(19);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.labels.len() as u16);
        for item in &self.labels {
            item.encode_into(encoder)?;
        }
        encoder.write_u16_be(self.next_cursor.len() as u16);
        for item in &self.next_cursor {
            encoder.write_byte(*item);
        }
        Ok(())
    }
}

impl ListLabelsResultOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 19u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 19, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut labels = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = LabelEntry::decode_with_decoder(decoder)?;
            labels.push(item);
        }
        let length = decoder.read_u16_be()? as usize;
        let next_cursor = decoder.read_bytes_vec(length)?;
        Ok(Self {
            message_type,
            op_id,
            labels,
            next_cursor,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ListLabelsResultInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ListLabelsResultInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ListLabelsResultOutput> for ListLabelsResultInput {
    fn from(o: ListLabelsResultOutput) -> Self {
        Self {
            op_id: o.op_id,
            labels: o.labels,
            next_cursor: o.next_cursor,
        }
    }
}

impl From<ListLabelsResultInput> for ListLabelsResultOutput {
    fn from(i: ListLabelsResultInput) -> Self {
        Self {
            message_type: 19u8,
            op_id: i.op_id,
            labels: i.labels,
            next_cursor: i.next_cursor,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchRequestInput {
    pub op_id: u32,
    pub labels: Vec<std::string::String>,
    pub since_generation: u64,
    pub since_envelope: Digest256,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchRequestOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub labels: Vec<std::string::String>,
    pub since_generation: u64,
    pub since_envelope: Digest256,
}

pub type FetchRequest = FetchRequestOutput;

impl FetchRequestInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(32);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.labels.len() as u16);
        for item in &self.labels {
            encoder.write_u16_be(item.len() as u16);
            for b in item.as_bytes() {
                encoder.write_byte(*b);
            }
        }
        encoder.write_u64_be(self.since_generation);
        self.since_envelope.encode_into(encoder)?;
        Ok(())
    }
}

impl FetchRequestOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 32u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 32, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut labels = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let str_len = decoder.read_u16_be()? as usize;
            let str_bytes = decoder.read_bytes_vec(str_len)?;
            let item = std::string::String::from_utf8(str_bytes)
                .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
            labels.push(item);
        }
        let since_generation = decoder.read_u64_be()?;
        let since_envelope = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            message_type,
            op_id,
            labels,
            since_generation,
            since_envelope,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        FetchRequestInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        FetchRequestInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<FetchRequestOutput> for FetchRequestInput {
    fn from(o: FetchRequestOutput) -> Self {
        Self {
            op_id: o.op_id,
            labels: o.labels,
            since_generation: o.since_generation,
            since_envelope: o.since_envelope,
        }
    }
}

impl From<FetchRequestInput> for FetchRequestOutput {
    fn from(i: FetchRequestInput) -> Self {
        Self {
            message_type: 32u8,
            op_id: i.op_id,
            labels: i.labels,
            since_generation: i.since_generation,
            since_envelope: i.since_envelope,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectBatchInput {
    pub op_id: u32,
    pub sequence: u32,
    pub objects: Vec<ObjectEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectBatchOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub sequence: u32,
    pub objects: Vec<ObjectEntry>,
}

pub type ObjectBatch = ObjectBatchOutput;

impl ObjectBatchInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(33);
        encoder.write_u32_be(self.op_id);
        encoder.write_u32_be(self.sequence);
        encoder.write_u16_be(self.objects.len() as u16);
        for item in &self.objects {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ObjectBatchOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 33u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 33, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let sequence = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut objects = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = ObjectEntry::decode_with_decoder(decoder)?;
            objects.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            sequence,
            objects,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ObjectBatchInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ObjectBatchInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ObjectBatchOutput> for ObjectBatchInput {
    fn from(o: ObjectBatchOutput) -> Self {
        Self {
            op_id: o.op_id,
            sequence: o.sequence,
            objects: o.objects,
        }
    }
}

impl From<ObjectBatchInput> for ObjectBatchOutput {
    fn from(i: ObjectBatchInput) -> Self {
        Self {
            message_type: 33u8,
            op_id: i.op_id,
            sequence: i.sequence,
            objects: i.objects,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectWantInput {
    pub op_id: u32,
    pub ids: Vec<Digest256>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectWantOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub ids: Vec<Digest256>,
}

pub type ObjectWant = ObjectWantOutput;

impl ObjectWantInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(34);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.ids.len() as u16);
        for item in &self.ids {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ObjectWantOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 34u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 34, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut ids = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = Digest256::decode_with_decoder(decoder)?;
            ids.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            ids,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ObjectWantInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ObjectWantInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ObjectWantOutput> for ObjectWantInput {
    fn from(o: ObjectWantOutput) -> Self {
        Self {
            op_id: o.op_id,
            ids: o.ids,
        }
    }
}

impl From<ObjectWantInput> for ObjectWantOutput {
    fn from(i: ObjectWantInput) -> Self {
        Self {
            message_type: 34u8,
            op_id: i.op_id,
            ids: i.ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchCompleteInput {
    pub op_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchCompleteOutput {
    pub message_type: u8,
    pub op_id: u32,
}

pub type FetchComplete = FetchCompleteOutput;

impl FetchCompleteInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(35);
        encoder.write_u32_be(self.op_id);
        Ok(())
    }
}

impl FetchCompleteOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 35u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 35, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        Ok(Self {
            message_type,
            op_id,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        FetchCompleteInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        FetchCompleteInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<FetchCompleteOutput> for FetchCompleteInput {
    fn from(o: FetchCompleteOutput) -> Self {
        Self { op_id: o.op_id }
    }
}

impl From<FetchCompleteInput> for FetchCompleteOutput {
    fn from(i: FetchCompleteInput) -> Self {
        Self {
            message_type: 35u8,
            op_id: i.op_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentFetchRequestInput {
    pub op_id: u32,
    pub content_ids: Vec<Digest256>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentFetchRequestOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_ids: Vec<Digest256>,
}

pub type ContentFetchRequest = ContentFetchRequestOutput;

impl ContentFetchRequestInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(48);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.content_ids.len() as u16);
        for item in &self.content_ids {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ContentFetchRequestOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 48u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 48, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut content_ids = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = Digest256::decode_with_decoder(decoder)?;
            content_ids.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            content_ids,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentFetchRequestInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentFetchRequestInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentFetchRequestOutput> for ContentFetchRequestInput {
    fn from(o: ContentFetchRequestOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_ids: o.content_ids,
        }
    }
}

impl From<ContentFetchRequestInput> for ContentFetchRequestOutput {
    fn from(i: ContentFetchRequestInput) -> Self {
        Self {
            message_type: 48u8,
            op_id: i.op_id,
            content_ids: i.content_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentBatchInput {
    pub op_id: u32,
    pub sequence: u32,
    pub contents: Vec<ContentEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentBatchOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub sequence: u32,
    pub contents: Vec<ContentEntry>,
}

pub type ContentBatch = ContentBatchOutput;

impl ContentBatchInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(49);
        encoder.write_u32_be(self.op_id);
        encoder.write_u32_be(self.sequence);
        encoder.write_u16_be(self.contents.len() as u16);
        for item in &self.contents {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ContentBatchOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 49u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 49, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let sequence = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut contents = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = ContentEntry::decode_with_decoder(decoder)?;
            contents.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            sequence,
            contents,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentBatchInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentBatchInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentBatchOutput> for ContentBatchInput {
    fn from(o: ContentBatchOutput) -> Self {
        Self {
            op_id: o.op_id,
            sequence: o.sequence,
            contents: o.contents,
        }
    }
}

impl From<ContentBatchInput> for ContentBatchOutput {
    fn from(i: ContentBatchInput) -> Self {
        Self {
            message_type: 49u8,
            op_id: i.op_id,
            sequence: i.sequence,
            contents: i.contents,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentChunkInput {
    pub op_id: u32,
    pub content_id: Digest256,
    pub offset: u64,
    pub data: Vec<u8>,
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentChunkOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_id: Digest256,
    pub offset: u64,
    pub data: Vec<u8>,
    pub is_final: bool,
}

pub type ContentChunk = ContentChunkOutput;

impl ContentChunkInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(50);
        encoder.write_u32_be(self.op_id);
        self.content_id.encode_into(encoder)?;
        encoder.write_u64_be(self.offset);
        encoder.write_u32_be(self.data.len() as u32);
        for item in &self.data {
            encoder.write_byte(*item);
        }
        encoder.write_byte(if self.is_final { 1 } else { 0 });
        Ok(())
    }
}

impl ContentChunkOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 50u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 50, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let content_id = Digest256::decode_with_decoder(decoder)?;
        let offset = decoder.read_u64_be()?;
        let length = decoder.read_u32_be()? as usize;
        let data = decoder.read_bytes_vec(length)?;
        let is_final = decoder.read_byte()? != 0;
        Ok(Self {
            message_type,
            op_id,
            content_id,
            offset,
            data,
            is_final,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentChunkInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentChunkInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentChunkOutput> for ContentChunkInput {
    fn from(o: ContentChunkOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_id: o.content_id,
            offset: o.offset,
            data: o.data,
            is_final: o.is_final,
        }
    }
}

impl From<ContentChunkInput> for ContentChunkOutput {
    fn from(i: ContentChunkInput) -> Self {
        Self {
            message_type: 50u8,
            op_id: i.op_id,
            content_id: i.content_id,
            offset: i.offset,
            data: i.data,
            is_final: i.is_final,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentRedirectInput {
    pub op_id: u32,
    pub content_id: Digest256,
    pub url: std::string::String,
    pub expires_seconds: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentRedirectOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_id: Digest256,
    pub url: std::string::String,
    pub expires_seconds: u32,
}

pub type ContentRedirect = ContentRedirectOutput;

impl ContentRedirectInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(51);
        encoder.write_u32_be(self.op_id);
        self.content_id.encode_into(encoder)?;
        encoder.write_u16_be(self.url.len() as u16);
        let string_bytes: &[u8] = self.url.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        encoder.write_u32_be(self.expires_seconds);
        Ok(())
    }
}

impl ContentRedirectOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 51u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 51, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let content_id = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let url = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let expires_seconds = decoder.read_u32_be()?;
        Ok(Self {
            message_type,
            op_id,
            content_id,
            url,
            expires_seconds,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentRedirectInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentRedirectInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentRedirectOutput> for ContentRedirectInput {
    fn from(o: ContentRedirectOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_id: o.content_id,
            url: o.url,
            expires_seconds: o.expires_seconds,
        }
    }
}

impl From<ContentRedirectInput> for ContentRedirectOutput {
    fn from(i: ContentRedirectInput) -> Self {
        Self {
            message_type: 51u8,
            op_id: i.op_id,
            content_id: i.content_id,
            url: i.url,
            expires_seconds: i.expires_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishRequestInput {
    pub op_id: u32,
    pub publish_key: Uuid128,
    pub envelope_id: Digest256,
    pub expected_generation: u64,
    pub expected_envelope: Digest256,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishRequestOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub publish_key: Uuid128,
    pub envelope_id: Digest256,
    pub expected_generation: u64,
    pub expected_envelope: Digest256,
    pub force: bool,
}

pub type PublishRequest = PublishRequestOutput;

impl PublishRequestInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(64);
        encoder.write_u32_be(self.op_id);
        self.publish_key.encode_into(encoder)?;
        self.envelope_id.encode_into(encoder)?;
        encoder.write_u64_be(self.expected_generation);
        self.expected_envelope.encode_into(encoder)?;
        encoder.write_byte(if self.force { 1 } else { 0 });
        Ok(())
    }
}

impl PublishRequestOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 64u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 64, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let publish_key = Uuid128::decode_with_decoder(decoder)?;
        let envelope_id = Digest256::decode_with_decoder(decoder)?;
        let expected_generation = decoder.read_u64_be()?;
        let expected_envelope = Digest256::decode_with_decoder(decoder)?;
        let force = decoder.read_byte()? != 0;
        Ok(Self {
            message_type,
            op_id,
            publish_key,
            envelope_id,
            expected_generation,
            expected_envelope,
            force,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishRequestInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishRequestInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishRequestOutput> for PublishRequestInput {
    fn from(o: PublishRequestOutput) -> Self {
        Self {
            op_id: o.op_id,
            publish_key: o.publish_key,
            envelope_id: o.envelope_id,
            expected_generation: o.expected_generation,
            expected_envelope: o.expected_envelope,
            force: o.force,
        }
    }
}

impl From<PublishRequestInput> for PublishRequestOutput {
    fn from(i: PublishRequestInput) -> Self {
        Self {
            message_type: 64u8,
            op_id: i.op_id,
            publish_key: i.publish_key,
            envelope_id: i.envelope_id,
            expected_generation: i.expected_generation,
            expected_envelope: i.expected_envelope,
            force: i.force,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectsNeededInput {
    pub op_id: u32,
    pub ids: Vec<Digest256>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectsNeededOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub ids: Vec<Digest256>,
}

pub type ObjectsNeeded = ObjectsNeededOutput;

impl ObjectsNeededInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(65);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.ids.len() as u16);
        for item in &self.ids {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ObjectsNeededOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 65u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 65, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut ids = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = Digest256::decode_with_decoder(decoder)?;
            ids.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            ids,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ObjectsNeededInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ObjectsNeededInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ObjectsNeededOutput> for ObjectsNeededInput {
    fn from(o: ObjectsNeededOutput) -> Self {
        Self {
            op_id: o.op_id,
            ids: o.ids,
        }
    }
}

impl From<ObjectsNeededInput> for ObjectsNeededOutput {
    fn from(i: ObjectsNeededInput) -> Self {
        Self {
            message_type: 65u8,
            op_id: i.op_id,
            ids: i.ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectsCompleteInput {
    pub op_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectsCompleteOutput {
    pub message_type: u8,
    pub op_id: u32,
}

pub type ObjectsComplete = ObjectsCompleteOutput;

impl ObjectsCompleteInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(66);
        encoder.write_u32_be(self.op_id);
        Ok(())
    }
}

impl ObjectsCompleteOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 66u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 66, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        Ok(Self {
            message_type,
            op_id,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ObjectsCompleteInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ObjectsCompleteInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ObjectsCompleteOutput> for ObjectsCompleteInput {
    fn from(o: ObjectsCompleteOutput) -> Self {
        Self { op_id: o.op_id }
    }
}

impl From<ObjectsCompleteInput> for ObjectsCompleteOutput {
    fn from(i: ObjectsCompleteInput) -> Self {
        Self {
            message_type: 66u8,
            op_id: i.op_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentNeededInput {
    pub op_id: u32,
    pub content_ids: Vec<Digest256>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentNeededOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_ids: Vec<Digest256>,
}

pub type ContentNeeded = ContentNeededOutput;

impl ContentNeededInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(67);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.content_ids.len() as u16);
        for item in &self.content_ids {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ContentNeededOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 67u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 67, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let mut content_ids = Vec::with_capacity(decoder.capacity_hint(length));
        for _ in 0..length {
            let item = Digest256::decode_with_decoder(decoder)?;
            content_ids.push(item);
        }
        Ok(Self {
            message_type,
            op_id,
            content_ids,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentNeededInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentNeededInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentNeededOutput> for ContentNeededInput {
    fn from(o: ContentNeededOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_ids: o.content_ids,
        }
    }
}

impl From<ContentNeededInput> for ContentNeededOutput {
    fn from(i: ContentNeededInput) -> Self {
        Self {
            message_type: 67u8,
            op_id: i.op_id,
            content_ids: i.content_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadInput {
    pub op_id: u32,
    pub content_id: Digest256,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_id: Digest256,
    pub data: Vec<u8>,
}

pub type ContentUpload = ContentUploadOutput;

impl ContentUploadInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(68);
        encoder.write_u32_be(self.op_id);
        self.content_id.encode_into(encoder)?;
        encoder.write_u32_be(self.data.len() as u32);
        for item in &self.data {
            encoder.write_byte(*item);
        }
        Ok(())
    }
}

impl ContentUploadOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 68u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 68, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let content_id = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let data = decoder.read_bytes_vec(length)?;
        Ok(Self {
            message_type,
            op_id,
            content_id,
            data,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentUploadInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentUploadInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentUploadOutput> for ContentUploadInput {
    fn from(o: ContentUploadOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_id: o.content_id,
            data: o.data,
        }
    }
}

impl From<ContentUploadInput> for ContentUploadOutput {
    fn from(i: ContentUploadInput) -> Self {
        Self {
            message_type: 68u8,
            op_id: i.op_id,
            content_id: i.content_id,
            data: i.data,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadChunkInput {
    pub op_id: u32,
    pub content_id: Digest256,
    pub offset: u64,
    pub data: Vec<u8>,
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadChunkOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub content_id: Digest256,
    pub offset: u64,
    pub data: Vec<u8>,
    pub is_final: bool,
}

pub type ContentUploadChunk = ContentUploadChunkOutput;

impl ContentUploadChunkInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(69);
        encoder.write_u32_be(self.op_id);
        self.content_id.encode_into(encoder)?;
        encoder.write_u64_be(self.offset);
        encoder.write_u32_be(self.data.len() as u32);
        for item in &self.data {
            encoder.write_byte(*item);
        }
        encoder.write_byte(if self.is_final { 1 } else { 0 });
        Ok(())
    }
}

impl ContentUploadChunkOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 69u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 69, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let content_id = Digest256::decode_with_decoder(decoder)?;
        let offset = decoder.read_u64_be()?;
        let length = decoder.read_u32_be()? as usize;
        let data = decoder.read_bytes_vec(length)?;
        let is_final = decoder.read_byte()? != 0;
        Ok(Self {
            message_type,
            op_id,
            content_id,
            offset,
            data,
            is_final,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentUploadChunkInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentUploadChunkInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentUploadChunkOutput> for ContentUploadChunkInput {
    fn from(o: ContentUploadChunkOutput) -> Self {
        Self {
            op_id: o.op_id,
            content_id: o.content_id,
            offset: o.offset,
            data: o.data,
            is_final: o.is_final,
        }
    }
}

impl From<ContentUploadChunkInput> for ContentUploadChunkOutput {
    fn from(i: ContentUploadChunkInput) -> Self {
        Self {
            message_type: 69u8,
            op_id: i.op_id,
            content_id: i.content_id,
            offset: i.offset,
            data: i.data,
            is_final: i.is_final,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadCompleteInput {
    pub op_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentUploadCompleteOutput {
    pub message_type: u8,
    pub op_id: u32,
}

pub type ContentUploadComplete = ContentUploadCompleteOutput;

impl ContentUploadCompleteInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(70);
        encoder.write_u32_be(self.op_id);
        Ok(())
    }
}

impl ContentUploadCompleteOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 70u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 70, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        Ok(Self {
            message_type,
            op_id,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentUploadCompleteInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentUploadCompleteInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentUploadCompleteOutput> for ContentUploadCompleteInput {
    fn from(o: ContentUploadCompleteOutput) -> Self {
        Self { op_id: o.op_id }
    }
}

impl From<ContentUploadCompleteInput> for ContentUploadCompleteOutput {
    fn from(i: ContentUploadCompleteInput) -> Self {
        Self {
            message_type: 70u8,
            op_id: i.op_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishCommitInput {
    pub op_id: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishCommitOutput {
    pub message_type: u8,
    pub op_id: u32,
}

pub type PublishCommit = PublishCommitOutput;

impl PublishCommitInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(71);
        encoder.write_u32_be(self.op_id);
        Ok(())
    }
}

impl PublishCommitOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 71u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 71, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        Ok(Self {
            message_type,
            op_id,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishCommitInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishCommitInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishCommitOutput> for PublishCommitInput {
    fn from(o: PublishCommitOutput) -> Self {
        Self { op_id: o.op_id }
    }
}

impl From<PublishCommitInput> for PublishCommitOutput {
    fn from(i: PublishCommitInput) -> Self {
        Self {
            message_type: 71u8,
            op_id: i.op_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishProgressInput {
    pub op_id: u32,
    pub phase: std::string::String,
    pub percent: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishProgressOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub phase: std::string::String,
    pub percent: u8,
}

pub type PublishProgress = PublishProgressOutput;

impl PublishProgressInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(72);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.phase.len() as u16);
        let string_bytes: &[u8] = self.phase.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        encoder.write_byte(self.percent);
        Ok(())
    }
}

impl PublishProgressOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 72u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 72, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let phase = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let percent = decoder.read_byte()?;
        Ok(Self {
            message_type,
            op_id,
            phase,
            percent,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishProgressInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishProgressInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishProgressOutput> for PublishProgressInput {
    fn from(o: PublishProgressOutput) -> Self {
        Self {
            op_id: o.op_id,
            phase: o.phase,
            percent: o.percent,
        }
    }
}

impl From<PublishProgressInput> for PublishProgressOutput {
    fn from(i: PublishProgressInput) -> Self {
        Self {
            message_type: 72u8,
            op_id: i.op_id,
            phase: i.phase,
            percent: i.percent,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PublishResultStatus {
    PublishStatusOk(PublishStatusOkOutput),
    PublishStatusStaleHead(PublishStatusStaleHeadOutput),
    PublishStatusRejected(PublishStatusRejectedOutput),
    PublishStatusAlreadyCommitted(PublishStatusAlreadyCommittedOutput),
    PublishStatusError(PublishStatusErrorOutput),
}

impl PublishResultStatus {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            PublishResultStatus::PublishStatusOk(v) => {
                encoder.write_uint8(v.code);
            }
            PublishResultStatus::PublishStatusStaleHead(v) => {
                encoder.write_uint8(v.code);
            }
            PublishResultStatus::PublishStatusRejected(v) => {
                encoder.write_uint8(v.code);
            }
            PublishResultStatus::PublishStatusAlreadyCommitted(v) => {
                encoder.write_uint8(v.code);
            }
            PublishResultStatus::PublishStatusError(v) => {
                encoder.write_uint8(v.code);
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.peek_uint8()?;
        // Match on discriminator value
        if value == 0 {
            Ok(PublishResultStatus::PublishStatusOk(
                PublishStatusOkOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(PublishResultStatus::PublishStatusStaleHead(
                PublishStatusStaleHeadOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 2 {
            Ok(PublishResultStatus::PublishStatusRejected(
                PublishStatusRejectedOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 3 {
            Ok(PublishResultStatus::PublishStatusAlreadyCommitted(
                PublishStatusAlreadyCommittedOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 4 {
            Ok(PublishResultStatus::PublishStatusError(
                PublishStatusErrorOutput::decode_with_decoder(decoder)?,
            ))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "unknown discriminator value: {}",
                value
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusOkInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusOkOutput {
    pub code: u8,
}

pub type PublishStatusOk = PublishStatusOkOutput;

impl PublishStatusOkInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        Ok(())
    }
}

impl PublishStatusOkOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishStatusOkInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishStatusOkInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishStatusOkOutput> for PublishStatusOkInput {
    fn from(_o: PublishStatusOkOutput) -> Self {
        Self {}
    }
}

impl From<PublishStatusOkInput> for PublishStatusOkOutput {
    fn from(_i: PublishStatusOkInput) -> Self {
        Self { code: 0u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusStaleHeadInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusStaleHeadOutput {
    pub code: u8,
}

pub type PublishStatusStaleHead = PublishStatusStaleHeadOutput;

impl PublishStatusStaleHeadInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        Ok(())
    }
}

impl PublishStatusStaleHeadOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishStatusStaleHeadInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishStatusStaleHeadInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishStatusStaleHeadOutput> for PublishStatusStaleHeadInput {
    fn from(_o: PublishStatusStaleHeadOutput) -> Self {
        Self {}
    }
}

impl From<PublishStatusStaleHeadInput> for PublishStatusStaleHeadOutput {
    fn from(_i: PublishStatusStaleHeadInput) -> Self {
        Self { code: 1u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusRejectedInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusRejectedOutput {
    pub code: u8,
}

pub type PublishStatusRejected = PublishStatusRejectedOutput;

impl PublishStatusRejectedInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(2);
        Ok(())
    }
}

impl PublishStatusRejectedOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 2u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishStatusRejectedInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishStatusRejectedInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishStatusRejectedOutput> for PublishStatusRejectedInput {
    fn from(_o: PublishStatusRejectedOutput) -> Self {
        Self {}
    }
}

impl From<PublishStatusRejectedInput> for PublishStatusRejectedOutput {
    fn from(_i: PublishStatusRejectedInput) -> Self {
        Self { code: 2u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusAlreadyCommittedInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusAlreadyCommittedOutput {
    pub code: u8,
}

pub type PublishStatusAlreadyCommitted = PublishStatusAlreadyCommittedOutput;

impl PublishStatusAlreadyCommittedInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(3);
        Ok(())
    }
}

impl PublishStatusAlreadyCommittedOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 3u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishStatusAlreadyCommittedInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishStatusAlreadyCommittedInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishStatusAlreadyCommittedOutput> for PublishStatusAlreadyCommittedInput {
    fn from(_o: PublishStatusAlreadyCommittedOutput) -> Self {
        Self {}
    }
}

impl From<PublishStatusAlreadyCommittedInput> for PublishStatusAlreadyCommittedOutput {
    fn from(_i: PublishStatusAlreadyCommittedInput) -> Self {
        Self { code: 3u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusErrorInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishStatusErrorOutput {
    pub code: u8,
}

pub type PublishStatusError = PublishStatusErrorOutput;

impl PublishStatusErrorInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(4);
        Ok(())
    }
}

impl PublishStatusErrorOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 4u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 4, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishStatusErrorInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishStatusErrorInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishStatusErrorOutput> for PublishStatusErrorInput {
    fn from(_o: PublishStatusErrorOutput) -> Self {
        Self {}
    }
}

impl From<PublishStatusErrorInput> for PublishStatusErrorOutput {
    fn from(_i: PublishStatusErrorInput) -> Self {
        Self { code: 4u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishResultInput {
    pub op_id: u32,
    pub receipt_id: Uuid128,
    pub status: PublishResultStatus,
    pub new_generation: u64,
    pub new_envelope: Digest256,
    pub message: std::string::String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishResultOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub receipt_id: Uuid128,
    pub status: PublishResultStatus,
    pub new_generation: u64,
    pub new_envelope: Digest256,
    pub message: std::string::String,
}

pub type PublishResult = PublishResultOutput;

impl PublishResultInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(73);
        encoder.write_u32_be(self.op_id);
        self.receipt_id.encode_into(encoder)?;
        self.status.encode_into(encoder)?;
        encoder.write_u64_be(self.new_generation);
        self.new_envelope.encode_into(encoder)?;
        encoder.write_u16_be(self.message.len() as u16);
        let string_bytes: &[u8] = self.message.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        Ok(())
    }
}

impl PublishResultOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 73u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 73, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let receipt_id = Uuid128::decode_with_decoder(decoder)?;
        let status = PublishResultStatus::decode_with_decoder(decoder)?;
        let new_generation = decoder.read_u64_be()?;
        let new_envelope = Digest256::decode_with_decoder(decoder)?;
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let message = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        Ok(Self {
            message_type,
            op_id,
            receipt_id,
            status,
            new_generation,
            new_envelope,
            message,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        PublishResultInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        PublishResultInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<PublishResultOutput> for PublishResultInput {
    fn from(o: PublishResultOutput) -> Self {
        Self {
            op_id: o.op_id,
            receipt_id: o.receipt_id,
            status: o.status,
            new_generation: o.new_generation,
            new_envelope: o.new_envelope,
            message: o.message,
        }
    }
}

impl From<PublishResultInput> for PublishResultOutput {
    fn from(i: PublishResultInput) -> Self {
        Self {
            message_type: 73u8,
            op_id: i.op_id,
            receipt_id: i.receipt_id,
            status: i.status,
            new_generation: i.new_generation,
            new_envelope: i.new_envelope,
            message: i.message,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreditInput {
    pub op_id: u32,
    pub ack_through_sequence: u32,
    pub window_bytes: u32,
    pub window_messages: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreditOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub ack_through_sequence: u32,
    pub window_bytes: u32,
    pub window_messages: u16,
}

pub type Credit = CreditOutput;

impl CreditInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(96);
        encoder.write_u32_be(self.op_id);
        encoder.write_u32_be(self.ack_through_sequence);
        encoder.write_u32_be(self.window_bytes);
        encoder.write_u16_be(self.window_messages);
        Ok(())
    }
}

impl CreditOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 96u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 96, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let ack_through_sequence = decoder.read_u32_be()?;
        let window_bytes = decoder.read_u32_be()?;
        let window_messages = decoder.read_u16_be()?;
        Ok(Self {
            message_type,
            op_id,
            ack_through_sequence,
            window_bytes,
            window_messages,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        CreditInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        CreditInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<CreditOutput> for CreditInput {
    fn from(o: CreditOutput) -> Self {
        Self {
            op_id: o.op_id,
            ack_through_sequence: o.ack_through_sequence,
            window_bytes: o.window_bytes,
            window_messages: o.window_messages,
        }
    }
}

impl From<CreditInput> for CreditOutput {
    fn from(i: CreditInput) -> Self {
        Self {
            message_type: 96u8,
            op_id: i.op_id,
            ack_through_sequence: i.ack_through_sequence,
            window_bytes: i.window_bytes,
            window_messages: i.window_messages,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ErrorScope {
    ErrorScopeConnection(ErrorScopeConnectionOutput),
    ErrorScopeOperation(ErrorScopeOperationOutput),
}

impl ErrorScope {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            ErrorScope::ErrorScopeConnection(v) => {
                encoder.write_uint8(v.code);
            }
            ErrorScope::ErrorScopeOperation(v) => {
                encoder.write_uint8(v.code);
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.peek_uint8()?;
        // Match on discriminator value
        if value == 0 {
            Ok(ErrorScope::ErrorScopeConnection(
                ErrorScopeConnectionOutput::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(ErrorScope::ErrorScopeOperation(
                ErrorScopeOperationOutput::decode_with_decoder(decoder)?,
            ))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "unknown discriminator value: {}",
                value
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorScopeConnectionInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorScopeConnectionOutput {
    pub code: u8,
}

pub type ErrorScopeConnection = ErrorScopeConnectionOutput;

impl ErrorScopeConnectionInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        Ok(())
    }
}

impl ErrorScopeConnectionOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ErrorScopeConnectionInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ErrorScopeConnectionInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ErrorScopeConnectionOutput> for ErrorScopeConnectionInput {
    fn from(_o: ErrorScopeConnectionOutput) -> Self {
        Self {}
    }
}

impl From<ErrorScopeConnectionInput> for ErrorScopeConnectionOutput {
    fn from(_i: ErrorScopeConnectionInput) -> Self {
        Self { code: 0u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorScopeOperationInput {}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorScopeOperationOutput {
    pub code: u8,
}

pub type ErrorScopeOperation = ErrorScopeOperationOutput;

impl ErrorScopeOperationInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        Ok(())
    }
}

impl ErrorScopeOperationOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let code = decoder.read_byte()?;
        if code != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                code
            )));
        }
        Ok(Self { code })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ErrorScopeOperationInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ErrorScopeOperationInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ErrorScopeOperationOutput> for ErrorScopeOperationInput {
    fn from(_o: ErrorScopeOperationOutput) -> Self {
        Self {}
    }
}

impl From<ErrorScopeOperationInput> for ErrorScopeOperationOutput {
    fn from(_i: ErrorScopeOperationInput) -> Self {
        Self { code: 1u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorInput {
    pub op_id: u32,
    pub code: u16,
    pub scope: ErrorScope,
    pub retryable: bool,
    pub message: std::string::String,
    pub object_id: Digest256,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorOutput {
    pub message_type: u8,
    pub op_id: u32,
    pub code: u16,
    pub scope: ErrorScope,
    pub retryable: bool,
    pub message: std::string::String,
    pub object_id: Digest256,
}

pub type Error = ErrorOutput;

impl ErrorInput {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(112);
        encoder.write_u32_be(self.op_id);
        encoder.write_u16_be(self.code);
        self.scope.encode_into(encoder)?;
        encoder.write_byte(if self.retryable { 1 } else { 0 });
        encoder.write_u16_be(self.message.len() as u16);
        let string_bytes: &[u8] = self.message.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.object_id.encode_into(encoder)?;
        Ok(())
    }
}

impl ErrorOutput {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let message_type = decoder.read_byte()?;
        if message_type != 112u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 112, got {}",
                message_type
            )));
        }
        let op_id = decoder.read_u32_be()?;
        let code = decoder.read_u16_be()?;
        let scope = ErrorScope::decode_with_decoder(decoder)?;
        let retryable = decoder.read_byte()? != 0;
        let length = decoder.read_u16_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let message = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let object_id = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            message_type,
            op_id,
            code,
            scope,
            retryable,
            message,
            object_id,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ErrorInput::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ErrorInput::from(self.clone()).encode_into(encoder)
    }
}

impl From<ErrorOutput> for ErrorInput {
    fn from(o: ErrorOutput) -> Self {
        Self {
            op_id: o.op_id,
            code: o.code,
            scope: o.scope,
            retryable: o.retryable,
            message: o.message,
            object_id: o.object_id,
        }
    }
}

impl From<ErrorInput> for ErrorOutput {
    fn from(i: ErrorInput) -> Self {
        Self {
            message_type: 112u8,
            op_id: i.op_id,
            code: i.code,
            scope: i.scope,
            retryable: i.retryable,
            message: i.message,
            object_id: i.object_id,
        }
    }
}
