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
pub enum WheatObject {
    TreeV1(TreeV1Output),
    ChangeV1(ChangeV1Output),
    RevisionV1(RevisionV1Output),
    ApplicationV1(ApplicationV1Output),
    ConflictV1(ConflictV1Output),
    ViewV1(ViewV1Output),
    ContentManifestV1(ContentManifestV1Output),
    RepositoryStateV3(RepositoryStateV3Output),
    RevisionMapNodeV1(RevisionMapNodeV1Output),
    ApplicationMapNodeV1(ApplicationMapNodeV1Output),
    ConflictSetNodeV1(ConflictSetNodeV1Output),
    ContentSetNodeV1(ContentSetNodeV1Output),
    RetainedSetNodeV1(RetainedSetNodeV1Output),
    ViewLabelManifestV1(ViewLabelManifestV1Output),
    RepositoryEnvelopeV1(RepositoryEnvelopeV1Output),
    EditV1(EditV1Output),
    ChangeMapNodeV1(ChangeMapNodeV1Output),
    ViewLineageMapNodeV1(ViewLineageMapNodeV1Output),
}

impl WheatObject {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            WheatObject::TreeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.entries.len() as u32, Endianness::BigEndian);
                for item in &v.entries {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ChangeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.change_id.encode_into(encoder)?;
                v.metadata.encode_into(encoder)?;
            }
            WheatObject::RevisionV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.change_id.encode_into(encoder)?;
                v.metadata.encode_into(encoder)?;
                encoder.write_uint32(v.predecessors.len() as u32, Endianness::BigEndian);
                for item in &v.predecessors {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.dependencies.len() as u32, Endianness::BigEndian);
                for item in &v.dependencies {
                    item.encode_into(encoder)?;
                }
                v.authored_against.encode_into(encoder)?;
                encoder.write_uint32(v.edits.len() as u32, Endianness::BigEndian);
                for item in &v.edits {
                    item.encode_into(encoder)?;
                }
                v.transition.encode_into(encoder)?;
                v.resulting_tree.encode_into(encoder)?;
            }
            WheatObject::ApplicationV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.revision_id.encode_into(encoder)?;
                v.target_view.encode_into(encoder)?;
                v.result.encode_into(encoder)?;
                encoder.write_uint32(v.applied.len() as u32, Endianness::BigEndian);
                for item in &v.applied {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ConflictV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.target_view.encode_into(encoder)?;
                encoder.write_uint32(v.edits.len() as u32, Endianness::BigEndian);
                for item in &v.edits {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ViewV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.tree.encode_into(encoder)?;
                v.revisions.encode_into(encoder)?;
                v.applications.encode_into(encoder)?;
                v.unresolved_conflicts.encode_into(encoder)?;
                v.required_content.encode_into(encoder)?;
            }
            WheatObject::ContentManifestV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.content.encode_into(encoder)?;
            }
            WheatObject::RepositoryStateV3(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.base_view.encode_into(encoder)?;
                v.changes.encode_into(encoder)?;
                v.view_lineage.encode_into(encoder)?;
                v.required_content.encode_into(encoder)?;
                v.retained_changes.encode_into(encoder)?;
                v.retained_views.encode_into(encoder)?;
                v.retained_revisions.encode_into(encoder)?;
                v.retained_applications.encode_into(encoder)?;
            }
            WheatObject::RevisionMapNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint32(v.value.len() as u32, Endianness::BigEndian);
                for item in &v.value {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ApplicationMapNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint32(v.value.len() as u32, Endianness::BigEndian);
                for item in &v.value {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ConflictSetNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint8(if v.present { 1 } else { 0 });
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ContentSetNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint8(if v.present { 1 } else { 0 });
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::RetainedSetNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint8(if v.present { 1 } else { 0 });
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ViewLabelManifestV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.labels.len() as u32, Endianness::BigEndian);
                for item in &v.labels {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::RepositoryEnvelopeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.state.encode_into(encoder)?;
                v.labels.encode_into(encoder)?;
            }
            WheatObject::EditV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.path.len() as u32, Endianness::BigEndian);
                let string_bytes: &[u8] = v.path.as_bytes();
                for &b in string_bytes.iter() {
                    encoder.write_uint8(b);
                }
                v.operation.encode_into(encoder)?;
            }
            WheatObject::ChangeMapNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint32(v.value.len() as u32, Endianness::BigEndian);
                for item in &v.value {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ViewLineageMapNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.prefix.len() as u32, Endianness::BigEndian);
                for item in &v.prefix {
                    encoder.write_uint8(*item);
                }
                encoder.write_uint32(v.value.len() as u32, Endianness::BigEndian);
                for item in &v.value {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.peek_uint16(Endianness::BigEndian)?;
        // Match on discriminator value
        if value == 0x0101 {
            Ok(WheatObject::TreeV1(TreeV1Output::decode_with_decoder(
                decoder,
            )?))
        } else if value == 0x0201 {
            Ok(WheatObject::ChangeV1(ChangeV1Output::decode_with_decoder(
                decoder,
            )?))
        } else if value == 0x0301 {
            Ok(WheatObject::RevisionV1(
                RevisionV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0401 {
            Ok(WheatObject::ApplicationV1(
                ApplicationV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0501 {
            Ok(WheatObject::ConflictV1(
                ConflictV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0601 {
            Ok(WheatObject::ViewV1(ViewV1Output::decode_with_decoder(
                decoder,
            )?))
        } else if value == 0x0701 {
            Ok(WheatObject::ContentManifestV1(
                ContentManifestV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0803 {
            Ok(WheatObject::RepositoryStateV3(
                RepositoryStateV3Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0901 {
            Ok(WheatObject::RevisionMapNodeV1(
                RevisionMapNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0a01 {
            Ok(WheatObject::ApplicationMapNodeV1(
                ApplicationMapNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0b01 {
            Ok(WheatObject::ConflictSetNodeV1(
                ConflictSetNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0c01 {
            Ok(WheatObject::ContentSetNodeV1(
                ContentSetNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0d01 {
            Ok(WheatObject::RetainedSetNodeV1(
                RetainedSetNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0e01 {
            Ok(WheatObject::ViewLabelManifestV1(
                ViewLabelManifestV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x0f01 {
            Ok(WheatObject::RepositoryEnvelopeV1(
                RepositoryEnvelopeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x1001 {
            Ok(WheatObject::EditV1(EditV1Output::decode_with_decoder(
                decoder,
            )?))
        } else if value == 0x1101 {
            Ok(WheatObject::ChangeMapNodeV1(
                ChangeMapNodeV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 0x1201 {
            Ok(WheatObject::ViewLineageMapNodeV1(
                ViewLineageMapNodeV1Output::decode_with_decoder(decoder)?,
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
pub struct ContentId {
    pub algorithm: u8,
    pub digest: Digest256,
}

impl ContentId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.algorithm);
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let algorithm = decoder.read_byte()?;
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { algorithm, digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeId {
    pub uuid: Uuid128,
}

impl ChangeId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.uuid.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let uuid = Uuid128::decode_with_decoder(decoder)?;
        Ok(Self { uuid })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionId {
    pub digest: Digest256,
}

impl RevisionId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditId {
    pub digest: Digest256,
}

impl EditId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeId {
    pub digest: Digest256,
}

impl TreeId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeObjectId {
    pub digest: Digest256,
}

impl ChangeObjectId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationId {
    pub digest: Digest256,
}

impl ApplicationId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictId {
    pub digest: Digest256,
}

impl ConflictId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewId {
    pub digest: Digest256,
}

impl ViewId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentManifestId {
    pub digest: Digest256,
}

impl ContentManifestId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapId {
    pub digest: Digest256,
}

impl RevisionMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeMapId {
    pub digest: Digest256,
}

impl ChangeMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapId {
    pub digest: Digest256,
}

impl ApplicationMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetId {
    pub digest: Digest256,
}

impl ConflictSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetId {
    pub digest: Digest256,
}

impl ContentSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetId {
    pub digest: Digest256,
}

impl RetainedSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLineageMapId {
    pub digest: Digest256,
}

impl ViewLineageMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateId {
    pub digest: Digest256,
}

impl RepositoryStateId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLabelManifestId {
    pub digest: Digest256,
}

impl ViewLabelManifestId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeV1Input {
    pub entries: Vec<DirectoryEntryV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeV1Output {
    pub wire_tag: u16,
    pub entries: Vec<DirectoryEntryV1>,
}

pub type TreeV1 = TreeV1Output;

impl TreeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(257);
        encoder.write_u32_be(self.entries.len() as u32);
        for item in &self.entries {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl TreeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 257u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 257, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut entries = Vec::with_capacity(length);
        for _ in 0..length {
            let item = DirectoryEntryV1::decode_with_decoder(decoder)?;
            entries.push(item);
        }
        Ok(Self { wire_tag, entries })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        TreeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        TreeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<TreeV1Output> for TreeV1Input {
    fn from(o: TreeV1Output) -> Self {
        Self { entries: o.entries }
    }
}

impl From<TreeV1Input> for TreeV1Output {
    fn from(i: TreeV1Input) -> Self {
        Self {
            wire_tag: 257u16,
            entries: i.entries,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntryV1 {
    pub name: std::string::String,
    pub value: DirectoryEntryValueV1,
}

impl DirectoryEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.name.len() as u32);
        let string_bytes: &[u8] = self.name.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.value.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let name = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let value = DirectoryEntryValueV1::decode_with_decoder(decoder)?;
        Ok(Self { name, value })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DirectoryEntryValueV1 {
    FileEntryV1(FileEntryV1Output),
    DirectoryEntryRefV1(DirectoryEntryRefV1Output),
}

impl DirectoryEntryValueV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            DirectoryEntryValueV1::FileEntryV1(v) => {
                encoder.write_uint8(v.entry_kind);
                v.value.encode_into(encoder)?;
            }
            DirectoryEntryValueV1::DirectoryEntryRefV1(v) => {
                encoder.write_uint8(v.entry_kind);
                v.directory.encode_into(encoder)?;
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
            Ok(DirectoryEntryValueV1::FileEntryV1(
                FileEntryV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(DirectoryEntryValueV1::DirectoryEntryRefV1(
                DirectoryEntryRefV1Output::decode_with_decoder(decoder)?,
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
pub struct FileKindV1 {
    pub value: u8,
}

impl FileKindV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.value);
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let value = decoder.read_byte()?;
        Ok(Self { value })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileValueV1 {
    pub content: ContentId,
    pub kind: FileKindV1,
}

impl FileValueV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.content.encode_into(encoder)?;
        self.kind.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let content = ContentId::decode_with_decoder(decoder)?;
        let kind = FileKindV1::decode_with_decoder(decoder)?;
        Ok(Self { content, kind })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntryV1Input {
    pub value: FileValueV1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntryV1Output {
    pub entry_kind: u8,
    pub value: FileValueV1,
}

pub type FileEntryV1 = FileEntryV1Output;

impl FileEntryV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        self.value.encode_into(encoder)?;
        Ok(())
    }
}

impl FileEntryV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let entry_kind = decoder.read_byte()?;
        if entry_kind != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                entry_kind
            )));
        }
        let value = FileValueV1::decode_with_decoder(decoder)?;
        Ok(Self { entry_kind, value })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        FileEntryV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        FileEntryV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<FileEntryV1Output> for FileEntryV1Input {
    fn from(o: FileEntryV1Output) -> Self {
        Self { value: o.value }
    }
}

impl From<FileEntryV1Input> for FileEntryV1Output {
    fn from(i: FileEntryV1Input) -> Self {
        Self {
            entry_kind: 0u8,
            value: i.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntryRefV1Input {
    pub directory: TreeId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryEntryRefV1Output {
    pub entry_kind: u8,
    pub directory: TreeId,
}

pub type DirectoryEntryRefV1 = DirectoryEntryRefV1Output;

impl DirectoryEntryRefV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        self.directory.encode_into(encoder)?;
        Ok(())
    }
}

impl DirectoryEntryRefV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let entry_kind = decoder.read_byte()?;
        if entry_kind != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                entry_kind
            )));
        }
        let directory = TreeId::decode_with_decoder(decoder)?;
        Ok(Self {
            entry_kind,
            directory,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        DirectoryEntryRefV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        DirectoryEntryRefV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<DirectoryEntryRefV1Output> for DirectoryEntryRefV1Input {
    fn from(o: DirectoryEntryRefV1Output) -> Self {
        Self {
            directory: o.directory,
        }
    }
}

impl From<DirectoryEntryRefV1Input> for DirectoryEntryRefV1Output {
    fn from(i: DirectoryEntryRefV1Input) -> Self {
        Self {
            entry_kind: 1u8,
            directory: i.directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActorV1 {
    pub name: std::string::String,
    pub email: std::string::String,
}

impl ActorV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.name.len() as u32);
        let string_bytes: &[u8] = self.name.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        encoder.write_u32_be(self.email.len() as u32);
        let string_bytes: &[u8] = self.email.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let name = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let email = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        Ok(Self { name, email })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimestampV1 {
    pub seconds: i64,
    pub offset_minutes: i16,
}

impl TimestampV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u64_be(self.seconds as u64);
        encoder.write_u16_be(self.offset_minutes as u16);
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let seconds = decoder.read_u64_be()? as i64;
        let offset_minutes = decoder.read_u16_be()? as i16;
        Ok(Self {
            seconds,
            offset_minutes,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceProvenanceV1 {
    pub system: std::string::String,
    pub object_id: Vec<u8>,
    pub parent_object_ids: Vec<SourceObjectIdV1>,
}

impl SourceProvenanceV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.system.len() as u32);
        let string_bytes: &[u8] = self.system.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        encoder.write_u32_be(self.object_id.len() as u32);
        for item in &self.object_id {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.parent_object_ids.len() as u32);
        for item in &self.parent_object_ids {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let system = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let length = decoder.read_u32_be()? as usize;
        let mut object_id = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            object_id.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut parent_object_ids = Vec::with_capacity(length);
        for _ in 0..length {
            let item = SourceObjectIdV1::decode_with_decoder(decoder)?;
            parent_object_ids.push(item);
        }
        Ok(Self {
            system,
            object_id,
            parent_object_ids,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceObjectIdV1 {
    pub bytes: Vec<u8>,
}

impl SourceObjectIdV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.bytes.len() as u32);
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
        let length = decoder.read_u32_be()? as usize;
        let mut bytes = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            bytes.push(item);
        }
        Ok(Self { bytes })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeMetadataV1 {
    pub title: std::string::String,
    pub description: std::string::String,
    pub creator: ActorV1,
    pub created_at: TimestampV1,
    pub provenance: Option<SourceProvenanceV1>,
}

impl ChangeMetadataV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.title.len() as u32);
        let string_bytes: &[u8] = self.title.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        encoder.write_u32_be(self.description.len() as u32);
        let string_bytes: &[u8] = self.description.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.creator.encode_into(encoder)?;
        self.created_at.encode_into(encoder)?;
        if let Some(ref v) = self.provenance {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let title = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let description = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let creator = ActorV1::decode_with_decoder(decoder)?;
        let created_at = TimestampV1::decode_with_decoder(decoder)?;
        let has_value = decoder.read_uint8()? != 0;
        let provenance = if has_value {
            Some(SourceProvenanceV1::decode_with_decoder(decoder)?)
        } else {
            None
        };
        Ok(Self {
            title,
            description,
            creator,
            created_at,
            provenance,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMetadataV1 {
    pub message: std::string::String,
    pub author: ActorV1,
    pub authored_at: TimestampV1,
    pub committer: ActorV1,
    pub committed_at: TimestampV1,
    pub provenance: Option<SourceProvenanceV1>,
}

impl RevisionMetadataV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.message.len() as u32);
        let string_bytes: &[u8] = self.message.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.author.encode_into(encoder)?;
        self.authored_at.encode_into(encoder)?;
        self.committer.encode_into(encoder)?;
        self.committed_at.encode_into(encoder)?;
        if let Some(ref v) = self.provenance {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let message = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let author = ActorV1::decode_with_decoder(decoder)?;
        let authored_at = TimestampV1::decode_with_decoder(decoder)?;
        let committer = ActorV1::decode_with_decoder(decoder)?;
        let committed_at = TimestampV1::decode_with_decoder(decoder)?;
        let has_value = decoder.read_uint8()? != 0;
        let provenance = if has_value {
            Some(SourceProvenanceV1::decode_with_decoder(decoder)?)
        } else {
            None
        };
        Ok(Self {
            message,
            author,
            authored_at,
            committer,
            committed_at,
            provenance,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeV1Input {
    pub change_id: ChangeId,
    pub metadata: ChangeMetadataV1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeV1Output {
    pub wire_tag: u16,
    pub change_id: ChangeId,
    pub metadata: ChangeMetadataV1,
}

pub type ChangeV1 = ChangeV1Output;

impl ChangeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(513);
        self.change_id.encode_into(encoder)?;
        self.metadata.encode_into(encoder)?;
        Ok(())
    }
}

impl ChangeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 513u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 513, got {}",
                wire_tag
            )));
        }
        let change_id = ChangeId::decode_with_decoder(decoder)?;
        let metadata = ChangeMetadataV1::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            change_id,
            metadata,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ChangeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ChangeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ChangeV1Output> for ChangeV1Input {
    fn from(o: ChangeV1Output) -> Self {
        Self {
            change_id: o.change_id,
            metadata: o.metadata,
        }
    }
}

impl From<ChangeV1Input> for ChangeV1Output {
    fn from(i: ChangeV1Input) -> Self {
        Self {
            wire_tag: 513u16,
            change_id: i.change_id,
            metadata: i.metadata,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionV1Input {
    pub change_id: ChangeId,
    pub metadata: RevisionMetadataV1,
    pub predecessors: Vec<RevisionId>,
    pub dependencies: Vec<ChangeId>,
    pub authored_against: ViewId,
    pub edits: Vec<EditId>,
    pub transition: EditTransitionV1,
    pub resulting_tree: TreeId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionV1Output {
    pub wire_tag: u16,
    pub change_id: ChangeId,
    pub metadata: RevisionMetadataV1,
    pub predecessors: Vec<RevisionId>,
    pub dependencies: Vec<ChangeId>,
    pub authored_against: ViewId,
    pub edits: Vec<EditId>,
    pub transition: EditTransitionV1,
    pub resulting_tree: TreeId,
}

pub type RevisionV1 = RevisionV1Output;

impl RevisionV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(769);
        self.change_id.encode_into(encoder)?;
        self.metadata.encode_into(encoder)?;
        encoder.write_u32_be(self.predecessors.len() as u32);
        for item in &self.predecessors {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.dependencies.len() as u32);
        for item in &self.dependencies {
            item.encode_into(encoder)?;
        }
        self.authored_against.encode_into(encoder)?;
        encoder.write_u32_be(self.edits.len() as u32);
        for item in &self.edits {
            item.encode_into(encoder)?;
        }
        self.transition.encode_into(encoder)?;
        self.resulting_tree.encode_into(encoder)?;
        Ok(())
    }
}

impl RevisionV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 769u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 769, got {}",
                wire_tag
            )));
        }
        let change_id = ChangeId::decode_with_decoder(decoder)?;
        let metadata = RevisionMetadataV1::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut predecessors = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RevisionId::decode_with_decoder(decoder)?;
            predecessors.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut dependencies = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ChangeId::decode_with_decoder(decoder)?;
            dependencies.push(item);
        }
        let authored_against = ViewId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut edits = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            edits.push(item);
        }
        let transition = EditTransitionV1::decode_with_decoder(decoder)?;
        let resulting_tree = TreeId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            change_id,
            metadata,
            predecessors,
            dependencies,
            authored_against,
            edits,
            transition,
            resulting_tree,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RevisionV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RevisionV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<RevisionV1Output> for RevisionV1Input {
    fn from(o: RevisionV1Output) -> Self {
        Self {
            change_id: o.change_id,
            metadata: o.metadata,
            predecessors: o.predecessors,
            dependencies: o.dependencies,
            authored_against: o.authored_against,
            edits: o.edits,
            transition: o.transition,
            resulting_tree: o.resulting_tree,
        }
    }
}

impl From<RevisionV1Input> for RevisionV1Output {
    fn from(i: RevisionV1Input) -> Self {
        Self {
            wire_tag: 769u16,
            change_id: i.change_id,
            metadata: i.metadata,
            predecessors: i.predecessors,
            dependencies: i.dependencies,
            authored_against: i.authored_against,
            edits: i.edits,
            transition: i.transition,
            resulting_tree: i.resulting_tree,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditTransitionV1 {
    pub preserved: Vec<EditId>,
    pub added: Vec<EditId>,
    pub removed: Vec<EditId>,
    pub subsumed: Vec<EditSubsumptionV1>,
    pub resolutions: Vec<ConflictResolutionEvidenceV1>,
}

impl EditTransitionV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.preserved.len() as u32);
        for item in &self.preserved {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.added.len() as u32);
        for item in &self.added {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.removed.len() as u32);
        for item in &self.removed {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.subsumed.len() as u32);
        for item in &self.subsumed {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.resolutions.len() as u32);
        for item in &self.resolutions {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let mut preserved = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            preserved.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut added = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            added.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut removed = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            removed.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut subsumed = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditSubsumptionV1::decode_with_decoder(decoder)?;
            subsumed.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut resolutions = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ConflictResolutionEvidenceV1::decode_with_decoder(decoder)?;
            resolutions.push(item);
        }
        Ok(Self {
            preserved,
            added,
            removed,
            subsumed,
            resolutions,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditSubsumptionV1 {
    pub source: EditId,
    pub replacements: Vec<EditId>,
}

impl EditSubsumptionV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.source.encode_into(encoder)?;
        encoder.write_u32_be(self.replacements.len() as u32);
        for item in &self.replacements {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let source = EditId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut replacements = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            replacements.push(item);
        }
        Ok(Self {
            source,
            replacements,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictResolutionEvidenceV1 {
    pub application: ApplicationId,
    pub conflicted_edits: Vec<EditId>,
    pub replacement_edits: Vec<EditId>,
}

impl ConflictResolutionEvidenceV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.application.encode_into(encoder)?;
        encoder.write_u32_be(self.conflicted_edits.len() as u32);
        for item in &self.conflicted_edits {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.replacement_edits.len() as u32);
        for item in &self.replacement_edits {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let application = ApplicationId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut conflicted_edits = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            conflicted_edits.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut replacement_edits = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            replacement_edits.push(item);
        }
        Ok(Self {
            application,
            conflicted_edits,
            replacement_edits,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationV1Input {
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub result: ApplicationResultV1,
    pub applied: Vec<AppliedEditEntryV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationV1Output {
    pub wire_tag: u16,
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub result: ApplicationResultV1,
    pub applied: Vec<AppliedEditEntryV1>,
}

pub type ApplicationV1 = ApplicationV1Output;

impl ApplicationV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(1025);
        self.revision_id.encode_into(encoder)?;
        self.target_view.encode_into(encoder)?;
        self.result.encode_into(encoder)?;
        encoder.write_u32_be(self.applied.len() as u32);
        for item in &self.applied {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ApplicationV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 1025u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1025, got {}",
                wire_tag
            )));
        }
        let revision_id = RevisionId::decode_with_decoder(decoder)?;
        let target_view = ViewId::decode_with_decoder(decoder)?;
        let result = ApplicationResultV1::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut applied = Vec::with_capacity(length);
        for _ in 0..length {
            let item = AppliedEditEntryV1::decode_with_decoder(decoder)?;
            applied.push(item);
        }
        Ok(Self {
            wire_tag,
            revision_id,
            target_view,
            result,
            applied,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ApplicationV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ApplicationV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ApplicationV1Output> for ApplicationV1Input {
    fn from(o: ApplicationV1Output) -> Self {
        Self {
            revision_id: o.revision_id,
            target_view: o.target_view,
            result: o.result,
            applied: o.applied,
        }
    }
}

impl From<ApplicationV1Input> for ApplicationV1Output {
    fn from(i: ApplicationV1Input) -> Self {
        Self {
            wire_tag: 1025u16,
            revision_id: i.revision_id,
            target_view: i.target_view,
            result: i.result,
            applied: i.applied,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplicationResultV1 {
    CleanApplicationV1(CleanApplicationV1Output),
    ConflictedApplicationV1(ConflictedApplicationV1Output),
}

impl ApplicationResultV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            ApplicationResultV1::CleanApplicationV1(v) => {
                encoder.write_uint8(v.result_kind);
                v.resulting_tree.encode_into(encoder)?;
            }
            ApplicationResultV1::ConflictedApplicationV1(v) => {
                encoder.write_uint8(v.result_kind);
                v.partial_tree.encode_into(encoder)?;
                v.conflict.encode_into(encoder)?;
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
            Ok(ApplicationResultV1::CleanApplicationV1(
                CleanApplicationV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(ApplicationResultV1::ConflictedApplicationV1(
                ConflictedApplicationV1Output::decode_with_decoder(decoder)?,
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
pub struct CleanApplicationV1Input {
    pub resulting_tree: TreeId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CleanApplicationV1Output {
    pub result_kind: u8,
    pub resulting_tree: TreeId,
}

pub type CleanApplicationV1 = CleanApplicationV1Output;

impl CleanApplicationV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        self.resulting_tree.encode_into(encoder)?;
        Ok(())
    }
}

impl CleanApplicationV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let result_kind = decoder.read_byte()?;
        if result_kind != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                result_kind
            )));
        }
        let resulting_tree = TreeId::decode_with_decoder(decoder)?;
        Ok(Self {
            result_kind,
            resulting_tree,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        CleanApplicationV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        CleanApplicationV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<CleanApplicationV1Output> for CleanApplicationV1Input {
    fn from(o: CleanApplicationV1Output) -> Self {
        Self {
            resulting_tree: o.resulting_tree,
        }
    }
}

impl From<CleanApplicationV1Input> for CleanApplicationV1Output {
    fn from(i: CleanApplicationV1Input) -> Self {
        Self {
            result_kind: 0u8,
            resulting_tree: i.resulting_tree,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictedApplicationV1Input {
    pub partial_tree: TreeId,
    pub conflict: ConflictId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictedApplicationV1Output {
    pub result_kind: u8,
    pub partial_tree: TreeId,
    pub conflict: ConflictId,
}

pub type ConflictedApplicationV1 = ConflictedApplicationV1Output;

impl ConflictedApplicationV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        self.partial_tree.encode_into(encoder)?;
        self.conflict.encode_into(encoder)?;
        Ok(())
    }
}

impl ConflictedApplicationV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let result_kind = decoder.read_byte()?;
        if result_kind != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                result_kind
            )));
        }
        let partial_tree = TreeId::decode_with_decoder(decoder)?;
        let conflict = ConflictId::decode_with_decoder(decoder)?;
        Ok(Self {
            result_kind,
            partial_tree,
            conflict,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ConflictedApplicationV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ConflictedApplicationV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ConflictedApplicationV1Output> for ConflictedApplicationV1Input {
    fn from(o: ConflictedApplicationV1Output) -> Self {
        Self {
            partial_tree: o.partial_tree,
            conflict: o.conflict,
        }
    }
}

impl From<ConflictedApplicationV1Input> for ConflictedApplicationV1Output {
    fn from(i: ConflictedApplicationV1Input) -> Self {
        Self {
            result_kind: 1u8,
            partial_tree: i.partial_tree,
            conflict: i.conflict,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictV1Input {
    pub target_view: ViewId,
    pub edits: Vec<FailedEditEntryV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictV1Output {
    pub wire_tag: u16,
    pub target_view: ViewId,
    pub edits: Vec<FailedEditEntryV1>,
}

pub type ConflictV1 = ConflictV1Output;

impl ConflictV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(1281);
        self.target_view.encode_into(encoder)?;
        encoder.write_u32_be(self.edits.len() as u32);
        for item in &self.edits {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ConflictV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 1281u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1281, got {}",
                wire_tag
            )));
        }
        let target_view = ViewId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut edits = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FailedEditEntryV1::decode_with_decoder(decoder)?;
            edits.push(item);
        }
        Ok(Self {
            wire_tag,
            target_view,
            edits,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ConflictV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ConflictV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ConflictV1Output> for ConflictV1Input {
    fn from(o: ConflictV1Output) -> Self {
        Self {
            target_view: o.target_view,
            edits: o.edits,
        }
    }
}

impl From<ConflictV1Input> for ConflictV1Output {
    fn from(i: ConflictV1Input) -> Self {
        Self {
            wire_tag: 1281u16,
            target_view: i.target_view,
            edits: i.edits,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ByteRangeV1 {
    pub start: u64,
    pub end: u64,
}

impl ByteRangeV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u64_be(self.start);
        encoder.write_u64_be(self.end);
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let start = decoder.read_u64_be()?;
        let end = decoder.read_u64_be()?;
        Ok(Self { start, end })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedEditEntryV1 {
    pub edit: EditId,
    pub evidence: AppliedEditEvidenceV1,
}

impl AppliedEditEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.edit.encode_into(encoder)?;
        self.evidence.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edit = EditId::decode_with_decoder(decoder)?;
        let evidence = AppliedEditEvidenceV1::decode_with_decoder(decoder)?;
        Ok(Self { edit, evidence })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedEditEvidenceV1 {
    pub target: Vec<FileValueV1>,
    pub read: Vec<ByteRangeV1>,
    pub write: Vec<ByteRangeV1>,
}

impl AppliedEditEvidenceV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.target.len() as u32);
        for item in &self.target {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.read.len() as u32);
        for item in &self.read {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.write.len() as u32);
        for item in &self.write {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let mut target = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FileValueV1::decode_with_decoder(decoder)?;
            target.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut read = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ByteRangeV1::decode_with_decoder(decoder)?;
            read.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut write = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ByteRangeV1::decode_with_decoder(decoder)?;
            write.push(item);
        }
        Ok(Self {
            target,
            read,
            write,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedEditEntryV1 {
    pub edit: EditId,
    pub evidence: FailedEditEvidenceV1,
}

impl FailedEditEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.edit.encode_into(encoder)?;
        self.evidence.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edit = EditId::decode_with_decoder(decoder)?;
        let evidence = FailedEditEvidenceV1::decode_with_decoder(decoder)?;
        Ok(Self { edit, evidence })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedEditEvidenceV1 {
    pub target: Vec<FileValueV1>,
    pub reason: EditFailureV1,
}

impl FailedEditEvidenceV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.target.len() as u32);
        for item in &self.target {
            item.encode_into(encoder)?;
        }
        self.reason.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let mut target = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FileValueV1::decode_with_decoder(decoder)?;
            target.push(item);
        }
        let reason = EditFailureV1::decode_with_decoder(decoder)?;
        Ok(Self { target, reason })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditFailureV1 {
    NoMatchFailureV1(NoMatchFailureV1Output),
    AmbiguousFailureV1(AmbiguousFailureV1Output),
    InterferenceFailureV1(InterferenceFailureV1Output),
    KindMismatchFailureV1(KindMismatchFailureV1Output),
    ExpectedFileMismatchFailureV1(ExpectedFileMismatchFailureV1Output),
}

impl EditFailureV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            EditFailureV1::NoMatchFailureV1(v) => {
                encoder.write_uint8(v.failure_kind);
            }
            EditFailureV1::AmbiguousFailureV1(v) => {
                encoder.write_uint8(v.failure_kind);
            }
            EditFailureV1::InterferenceFailureV1(v) => {
                encoder.write_uint8(v.failure_kind);
                encoder.write_uint32(v.with.len() as u32, Endianness::BigEndian);
                for item in &v.with {
                    item.encode_into(encoder)?;
                }
            }
            EditFailureV1::KindMismatchFailureV1(v) => {
                encoder.write_uint8(v.failure_kind);
            }
            EditFailureV1::ExpectedFileMismatchFailureV1(v) => {
                encoder.write_uint8(v.failure_kind);
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
            Ok(EditFailureV1::NoMatchFailureV1(
                NoMatchFailureV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(EditFailureV1::AmbiguousFailureV1(
                AmbiguousFailureV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 2 {
            Ok(EditFailureV1::InterferenceFailureV1(
                InterferenceFailureV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 3 {
            Ok(EditFailureV1::KindMismatchFailureV1(
                KindMismatchFailureV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 4 {
            Ok(EditFailureV1::ExpectedFileMismatchFailureV1(
                ExpectedFileMismatchFailureV1Output::decode_with_decoder(decoder)?,
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
pub struct NoMatchFailureV1Input {}

#[derive(Debug, Clone, PartialEq)]
pub struct NoMatchFailureV1Output {
    pub failure_kind: u8,
}

pub type NoMatchFailureV1 = NoMatchFailureV1Output;

impl NoMatchFailureV1Input {
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

impl NoMatchFailureV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let failure_kind = decoder.read_byte()?;
        if failure_kind != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                failure_kind
            )));
        }
        Ok(Self { failure_kind })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        NoMatchFailureV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        NoMatchFailureV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<NoMatchFailureV1Output> for NoMatchFailureV1Input {
    fn from(_o: NoMatchFailureV1Output) -> Self {
        Self {}
    }
}

impl From<NoMatchFailureV1Input> for NoMatchFailureV1Output {
    fn from(_i: NoMatchFailureV1Input) -> Self {
        Self { failure_kind: 0u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AmbiguousFailureV1Input {}

#[derive(Debug, Clone, PartialEq)]
pub struct AmbiguousFailureV1Output {
    pub failure_kind: u8,
}

pub type AmbiguousFailureV1 = AmbiguousFailureV1Output;

impl AmbiguousFailureV1Input {
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

impl AmbiguousFailureV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let failure_kind = decoder.read_byte()?;
        if failure_kind != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                failure_kind
            )));
        }
        Ok(Self { failure_kind })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        AmbiguousFailureV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        AmbiguousFailureV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<AmbiguousFailureV1Output> for AmbiguousFailureV1Input {
    fn from(_o: AmbiguousFailureV1Output) -> Self {
        Self {}
    }
}

impl From<AmbiguousFailureV1Input> for AmbiguousFailureV1Output {
    fn from(_i: AmbiguousFailureV1Input) -> Self {
        Self { failure_kind: 1u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterferenceFailureV1Input {
    pub with: Vec<EditId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterferenceFailureV1Output {
    pub failure_kind: u8,
    pub with: Vec<EditId>,
}

pub type InterferenceFailureV1 = InterferenceFailureV1Output;

impl InterferenceFailureV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(2);
        encoder.write_u32_be(self.with.len() as u32);
        for item in &self.with {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl InterferenceFailureV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let failure_kind = decoder.read_byte()?;
        if failure_kind != 2u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2, got {}",
                failure_kind
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut with = Vec::with_capacity(length);
        for _ in 0..length {
            let item = EditId::decode_with_decoder(decoder)?;
            with.push(item);
        }
        Ok(Self { failure_kind, with })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        InterferenceFailureV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        InterferenceFailureV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<InterferenceFailureV1Output> for InterferenceFailureV1Input {
    fn from(o: InterferenceFailureV1Output) -> Self {
        Self { with: o.with }
    }
}

impl From<InterferenceFailureV1Input> for InterferenceFailureV1Output {
    fn from(i: InterferenceFailureV1Input) -> Self {
        Self {
            failure_kind: 2u8,
            with: i.with,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KindMismatchFailureV1Input {}

#[derive(Debug, Clone, PartialEq)]
pub struct KindMismatchFailureV1Output {
    pub failure_kind: u8,
}

pub type KindMismatchFailureV1 = KindMismatchFailureV1Output;

impl KindMismatchFailureV1Input {
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

impl KindMismatchFailureV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let failure_kind = decoder.read_byte()?;
        if failure_kind != 3u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3, got {}",
                failure_kind
            )));
        }
        Ok(Self { failure_kind })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        KindMismatchFailureV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        KindMismatchFailureV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<KindMismatchFailureV1Output> for KindMismatchFailureV1Input {
    fn from(_o: KindMismatchFailureV1Output) -> Self {
        Self {}
    }
}

impl From<KindMismatchFailureV1Input> for KindMismatchFailureV1Output {
    fn from(_i: KindMismatchFailureV1Input) -> Self {
        Self { failure_kind: 3u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedFileMismatchFailureV1Input {}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedFileMismatchFailureV1Output {
    pub failure_kind: u8,
}

pub type ExpectedFileMismatchFailureV1 = ExpectedFileMismatchFailureV1Output;

impl ExpectedFileMismatchFailureV1Input {
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

impl ExpectedFileMismatchFailureV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let failure_kind = decoder.read_byte()?;
        if failure_kind != 4u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 4, got {}",
                failure_kind
            )));
        }
        Ok(Self { failure_kind })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ExpectedFileMismatchFailureV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ExpectedFileMismatchFailureV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ExpectedFileMismatchFailureV1Output> for ExpectedFileMismatchFailureV1Input {
    fn from(_o: ExpectedFileMismatchFailureV1Output) -> Self {
        Self {}
    }
}

impl From<ExpectedFileMismatchFailureV1Input> for ExpectedFileMismatchFailureV1Output {
    fn from(_i: ExpectedFileMismatchFailureV1Input) -> Self {
        Self { failure_kind: 4u8 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewV1Input {
    pub tree: TreeId,
    pub revisions: RevisionMapId,
    pub applications: ApplicationMapId,
    pub unresolved_conflicts: ConflictSetId,
    pub required_content: ContentManifestId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewV1Output {
    pub wire_tag: u16,
    pub tree: TreeId,
    pub revisions: RevisionMapId,
    pub applications: ApplicationMapId,
    pub unresolved_conflicts: ConflictSetId,
    pub required_content: ContentManifestId,
}

pub type ViewV1 = ViewV1Output;

impl ViewV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(1537);
        self.tree.encode_into(encoder)?;
        self.revisions.encode_into(encoder)?;
        self.applications.encode_into(encoder)?;
        self.unresolved_conflicts.encode_into(encoder)?;
        self.required_content.encode_into(encoder)?;
        Ok(())
    }
}

impl ViewV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 1537u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1537, got {}",
                wire_tag
            )));
        }
        let tree = TreeId::decode_with_decoder(decoder)?;
        let revisions = RevisionMapId::decode_with_decoder(decoder)?;
        let applications = ApplicationMapId::decode_with_decoder(decoder)?;
        let unresolved_conflicts = ConflictSetId::decode_with_decoder(decoder)?;
        let required_content = ContentManifestId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            tree,
            revisions,
            applications,
            unresolved_conflicts,
            required_content,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ViewV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ViewV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ViewV1Output> for ViewV1Input {
    fn from(o: ViewV1Output) -> Self {
        Self {
            tree: o.tree,
            revisions: o.revisions,
            applications: o.applications,
            unresolved_conflicts: o.unresolved_conflicts,
            required_content: o.required_content,
        }
    }
}

impl From<ViewV1Input> for ViewV1Output {
    fn from(i: ViewV1Input) -> Self {
        Self {
            wire_tag: 1537u16,
            tree: i.tree,
            revisions: i.revisions,
            applications: i.applications,
            unresolved_conflicts: i.unresolved_conflicts,
            required_content: i.required_content,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentManifestV1Input {
    pub content: ContentSetId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentManifestV1Output {
    pub wire_tag: u16,
    pub content: ContentSetId,
}

pub type ContentManifestV1 = ContentManifestV1Output;

impl ContentManifestV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(1793);
        self.content.encode_into(encoder)?;
        Ok(())
    }
}

impl ContentManifestV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 1793u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1793, got {}",
                wire_tag
            )));
        }
        let content = ContentSetId::decode_with_decoder(decoder)?;
        Ok(Self { wire_tag, content })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentManifestV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentManifestV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentManifestV1Output> for ContentManifestV1Input {
    fn from(o: ContentManifestV1Output) -> Self {
        Self { content: o.content }
    }
}

impl From<ContentManifestV1Input> for ContentManifestV1Output {
    fn from(i: ContentManifestV1Input) -> Self {
        Self {
            wire_tag: 1793u16,
            content: i.content,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapNodeV1Input {
    pub prefix: Vec<u8>,
    pub value: Vec<RevisionId>,
    pub children: Vec<RevisionMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub value: Vec<RevisionId>,
    pub children: Vec<RevisionMapChildV1>,
}

pub type RevisionMapNodeV1 = RevisionMapNodeV1Output;

impl RevisionMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(2305);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl RevisionMapNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 2305u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2305, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut value = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RevisionId::decode_with_decoder(decoder)?;
            value.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RevisionMapChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            value,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RevisionMapNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RevisionMapNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<RevisionMapNodeV1Output> for RevisionMapNodeV1Input {
    fn from(o: RevisionMapNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<RevisionMapNodeV1Input> for RevisionMapNodeV1Output {
    fn from(i: RevisionMapNodeV1Input) -> Self {
        Self {
            wire_tag: 2305u16,
            prefix: i.prefix,
            value: i.value,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapChildV1 {
    pub edge: u8,
    pub child: RevisionMapId,
}

impl RevisionMapChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = RevisionMapId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeMapNodeV1Input {
    pub prefix: Vec<u8>,
    pub value: Vec<ChangeObjectId>,
    pub children: Vec<ChangeMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeMapNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub value: Vec<ChangeObjectId>,
    pub children: Vec<ChangeMapChildV1>,
}

pub type ChangeMapNodeV1 = ChangeMapNodeV1Output;

impl ChangeMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(4353);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ChangeMapNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 4353u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 4353, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut value = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ChangeObjectId::decode_with_decoder(decoder)?;
            value.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ChangeMapChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            value,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ChangeMapNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ChangeMapNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ChangeMapNodeV1Output> for ChangeMapNodeV1Input {
    fn from(o: ChangeMapNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<ChangeMapNodeV1Input> for ChangeMapNodeV1Output {
    fn from(i: ChangeMapNodeV1Input) -> Self {
        Self {
            wire_tag: 4353u16,
            prefix: i.prefix,
            value: i.value,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeMapChildV1 {
    pub edge: u8,
    pub child: ChangeMapId,
}

impl ChangeMapChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ChangeMapId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapNodeV1Input {
    pub prefix: Vec<u8>,
    pub value: Vec<ApplicationId>,
    pub children: Vec<ApplicationMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub value: Vec<ApplicationId>,
    pub children: Vec<ApplicationMapChildV1>,
}

pub type ApplicationMapNodeV1 = ApplicationMapNodeV1Output;

impl ApplicationMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(2561);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ApplicationMapNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 2561u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2561, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut value = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ApplicationId::decode_with_decoder(decoder)?;
            value.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ApplicationMapChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            value,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ApplicationMapNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ApplicationMapNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ApplicationMapNodeV1Output> for ApplicationMapNodeV1Input {
    fn from(o: ApplicationMapNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<ApplicationMapNodeV1Input> for ApplicationMapNodeV1Output {
    fn from(i: ApplicationMapNodeV1Input) -> Self {
        Self {
            wire_tag: 2561u16,
            prefix: i.prefix,
            value: i.value,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapChildV1 {
    pub edge: u8,
    pub child: ApplicationMapId,
}

impl ApplicationMapChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ApplicationMapId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLineageMapNodeV1Input {
    pub prefix: Vec<u8>,
    pub value: Vec<RetainedSetId>,
    pub children: Vec<ViewLineageMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLineageMapNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub value: Vec<RetainedSetId>,
    pub children: Vec<ViewLineageMapChildV1>,
}

pub type ViewLineageMapNodeV1 = ViewLineageMapNodeV1Output;

impl ViewLineageMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(4609);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ViewLineageMapNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 4609u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 4609, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut value = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RetainedSetId::decode_with_decoder(decoder)?;
            value.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ViewLineageMapChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            value,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ViewLineageMapNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ViewLineageMapNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ViewLineageMapNodeV1Output> for ViewLineageMapNodeV1Input {
    fn from(o: ViewLineageMapNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<ViewLineageMapNodeV1Input> for ViewLineageMapNodeV1Output {
    fn from(i: ViewLineageMapNodeV1Input) -> Self {
        Self {
            wire_tag: 4609u16,
            prefix: i.prefix,
            value: i.value,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLineageMapChildV1 {
    pub edge: u8,
    pub child: ViewLineageMapId,
}

impl ViewLineageMapChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ViewLineageMapId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetNodeV1Input {
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<ConflictSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<ConflictSetChildV1>,
}

pub type ConflictSetNodeV1 = ConflictSetNodeV1Output;

impl ConflictSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(2817);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ConflictSetNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 2817u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2817, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ConflictSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            present,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ConflictSetNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ConflictSetNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ConflictSetNodeV1Output> for ConflictSetNodeV1Input {
    fn from(o: ConflictSetNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<ConflictSetNodeV1Input> for ConflictSetNodeV1Output {
    fn from(i: ConflictSetNodeV1Input) -> Self {
        Self {
            wire_tag: 2817u16,
            prefix: i.prefix,
            present: i.present,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetChildV1 {
    pub edge: u8,
    pub child: ConflictSetId,
}

impl ConflictSetChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ConflictSetId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetNodeV1Input {
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<ContentSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<ContentSetChildV1>,
}

pub type ContentSetNodeV1 = ContentSetNodeV1Output;

impl ContentSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(3073);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ContentSetNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 3073u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3073, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ContentSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            present,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentSetNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentSetNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ContentSetNodeV1Output> for ContentSetNodeV1Input {
    fn from(o: ContentSetNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<ContentSetNodeV1Input> for ContentSetNodeV1Output {
    fn from(i: ContentSetNodeV1Input) -> Self {
        Self {
            wire_tag: 3073u16,
            prefix: i.prefix,
            present: i.present,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetChildV1 {
    pub edge: u8,
    pub child: ContentSetId,
}

impl ContentSetChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ContentSetId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetNodeV1Input {
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<RetainedSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetNodeV1Output {
    pub wire_tag: u16,
    pub prefix: Vec<u8>,
    pub present: bool,
    pub children: Vec<RetainedSetChildV1>,
}

pub type RetainedSetNodeV1 = RetainedSetNodeV1Output;

impl RetainedSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(3329);
        encoder.write_u32_be(self.prefix.len() as u32);
        for item in &self.prefix {
            encoder.write_byte(*item);
        }
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl RetainedSetNodeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 3329u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3329, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut prefix = Vec::with_capacity(length);
        for _ in 0..length {
            let item = decoder.read_byte()?;
            prefix.push(item);
        }
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RetainedSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            prefix,
            present,
            children,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RetainedSetNodeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RetainedSetNodeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<RetainedSetNodeV1Output> for RetainedSetNodeV1Input {
    fn from(o: RetainedSetNodeV1Output) -> Self {
        Self {
            prefix: o.prefix,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<RetainedSetNodeV1Input> for RetainedSetNodeV1Output {
    fn from(i: RetainedSetNodeV1Input) -> Self {
        Self {
            wire_tag: 3329u16,
            prefix: i.prefix,
            present: i.present,
            children: i.children,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetChildV1 {
    pub edge: u8,
    pub child: RetainedSetId,
}

impl RetainedSetChildV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.edge);
        self.child.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = RetainedSetId::decode_with_decoder(decoder)?;
        Ok(Self { edge, child })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateV3Input {
    pub base_view: ViewId,
    pub changes: ChangeMapId,
    pub view_lineage: ViewLineageMapId,
    pub required_content: ContentManifestId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateV3Output {
    pub wire_tag: u16,
    pub base_view: ViewId,
    pub changes: ChangeMapId,
    pub view_lineage: ViewLineageMapId,
    pub required_content: ContentManifestId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
}

pub type RepositoryStateV3 = RepositoryStateV3Output;

impl RepositoryStateV3Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(2051);
        self.base_view.encode_into(encoder)?;
        self.changes.encode_into(encoder)?;
        self.view_lineage.encode_into(encoder)?;
        self.required_content.encode_into(encoder)?;
        self.retained_changes.encode_into(encoder)?;
        self.retained_views.encode_into(encoder)?;
        self.retained_revisions.encode_into(encoder)?;
        self.retained_applications.encode_into(encoder)?;
        Ok(())
    }
}

impl RepositoryStateV3Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 2051u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 2051, got {}",
                wire_tag
            )));
        }
        let base_view = ViewId::decode_with_decoder(decoder)?;
        let changes = ChangeMapId::decode_with_decoder(decoder)?;
        let view_lineage = ViewLineageMapId::decode_with_decoder(decoder)?;
        let required_content = ContentManifestId::decode_with_decoder(decoder)?;
        let retained_changes = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_views = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_revisions = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_applications = RetainedSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            base_view,
            changes,
            view_lineage,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RepositoryStateV3Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RepositoryStateV3Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<RepositoryStateV3Output> for RepositoryStateV3Input {
    fn from(o: RepositoryStateV3Output) -> Self {
        Self {
            base_view: o.base_view,
            changes: o.changes,
            view_lineage: o.view_lineage,
            required_content: o.required_content,
            retained_changes: o.retained_changes,
            retained_views: o.retained_views,
            retained_revisions: o.retained_revisions,
            retained_applications: o.retained_applications,
        }
    }
}

impl From<RepositoryStateV3Input> for RepositoryStateV3Output {
    fn from(i: RepositoryStateV3Input) -> Self {
        Self {
            wire_tag: 2051u16,
            base_view: i.base_view,
            changes: i.changes,
            view_lineage: i.view_lineage,
            required_content: i.required_content,
            retained_changes: i.retained_changes,
            retained_views: i.retained_views,
            retained_revisions: i.retained_revisions,
            retained_applications: i.retained_applications,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLabelManifestV1Input {
    pub labels: Vec<ViewLabelManifestEntryV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLabelManifestV1Output {
    pub wire_tag: u16,
    pub labels: Vec<ViewLabelManifestEntryV1>,
}

pub type ViewLabelManifestV1 = ViewLabelManifestV1Output;

impl ViewLabelManifestV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(3585);
        encoder.write_u32_be(self.labels.len() as u32);
        for item in &self.labels {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ViewLabelManifestV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 3585u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3585, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut labels = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ViewLabelManifestEntryV1::decode_with_decoder(decoder)?;
            labels.push(item);
        }
        Ok(Self { wire_tag, labels })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ViewLabelManifestV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ViewLabelManifestV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ViewLabelManifestV1Output> for ViewLabelManifestV1Input {
    fn from(o: ViewLabelManifestV1Output) -> Self {
        Self { labels: o.labels }
    }
}

impl From<ViewLabelManifestV1Input> for ViewLabelManifestV1Output {
    fn from(i: ViewLabelManifestV1Input) -> Self {
        Self {
            wire_tag: 3585u16,
            labels: i.labels,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewLabelManifestEntryV1 {
    pub name: std::string::String,
    pub view: ViewId,
    pub original_git_refs: Vec<std::string::String>,
}

impl ViewLabelManifestEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.name.len() as u32);
        let string_bytes: &[u8] = self.name.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.view.encode_into(encoder)?;
        encoder.write_u32_be(self.original_git_refs.len() as u32);
        for item in &self.original_git_refs {
            encoder.write_u32_be(item.len() as u32);
            for b in item.as_bytes() {
                encoder.write_byte(*b);
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let name = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let view = ViewId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut original_git_refs = Vec::with_capacity(length);
        for _ in 0..length {
            let str_len = decoder.read_u32_be()? as usize;
            let str_bytes = decoder.read_bytes_vec(str_len)?;
            let item = std::string::String::from_utf8(str_bytes)
                .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
            original_git_refs.push(item);
        }
        Ok(Self {
            name,
            view,
            original_git_refs,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryEnvelopeV1Input {
    pub state: RepositoryStateId,
    pub labels: ViewLabelManifestId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryEnvelopeV1Output {
    pub wire_tag: u16,
    pub state: RepositoryStateId,
    pub labels: ViewLabelManifestId,
}

pub type RepositoryEnvelopeV1 = RepositoryEnvelopeV1Output;

impl RepositoryEnvelopeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(3841);
        self.state.encode_into(encoder)?;
        self.labels.encode_into(encoder)?;
        Ok(())
    }
}

impl RepositoryEnvelopeV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 3841u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 3841, got {}",
                wire_tag
            )));
        }
        let state = RepositoryStateId::decode_with_decoder(decoder)?;
        let labels = ViewLabelManifestId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            state,
            labels,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RepositoryEnvelopeV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RepositoryEnvelopeV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<RepositoryEnvelopeV1Output> for RepositoryEnvelopeV1Input {
    fn from(o: RepositoryEnvelopeV1Output) -> Self {
        Self {
            state: o.state,
            labels: o.labels,
        }
    }
}

impl From<RepositoryEnvelopeV1Input> for RepositoryEnvelopeV1Output {
    fn from(i: RepositoryEnvelopeV1Input) -> Self {
        Self {
            wire_tag: 3841u16,
            state: i.state,
            labels: i.labels,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditV1Input {
    pub path: std::string::String,
    pub operation: EditOperationV1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditV1Output {
    pub wire_tag: u16,
    pub path: std::string::String,
    pub operation: EditOperationV1,
}

pub type EditV1 = EditV1Output;

impl EditV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u16_be(4097);
        encoder.write_u32_be(self.path.len() as u32);
        let string_bytes: &[u8] = self.path.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        self.operation.encode_into(encoder)?;
        Ok(())
    }
}

impl EditV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 4097u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 4097, got {}",
                wire_tag
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let path = std::string::String::from_utf8(bytes)
            .map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let operation = EditOperationV1::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            path,
            operation,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        EditV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        EditV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<EditV1Output> for EditV1Input {
    fn from(o: EditV1Output) -> Self {
        Self {
            path: o.path,
            operation: o.operation,
        }
    }
}

impl From<EditV1Input> for EditV1Output {
    fn from(i: EditV1Input) -> Self {
        Self {
            wire_tag: 4097u16,
            path: i.path,
            operation: i.operation,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditOperationV1 {
    GuardedTextEditV1(GuardedTextEditV1Output),
    ExactFileEditV1(ExactFileEditV1Output),
}

impl EditOperationV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        match self {
            EditOperationV1::GuardedTextEditV1(v) => {
                encoder.write_uint8(v.operation_kind);
                v.kind.encode_into(encoder)?;
                v.before.encode_into(encoder)?;
                v.expected.encode_into(encoder)?;
                v.replacement.encode_into(encoder)?;
                v.after.encode_into(encoder)?;
                encoder.write_uint8(if v.start_anchor { 1 } else { 0 });
                encoder.write_uint8(if v.end_anchor { 1 } else { 0 });
            }
            EditOperationV1::ExactFileEditV1(v) => {
                encoder.write_uint8(v.operation_kind);
                encoder.write_uint32(v.expected.len() as u32, Endianness::BigEndian);
                for item in &v.expected {
                    item.encode_into(encoder)?;
                }
                encoder.write_uint32(v.replacement.len() as u32, Endianness::BigEndian);
                for item in &v.replacement {
                    item.encode_into(encoder)?;
                }
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
            Ok(EditOperationV1::GuardedTextEditV1(
                GuardedTextEditV1Output::decode_with_decoder(decoder)?,
            ))
        } else if value == 1 {
            Ok(EditOperationV1::ExactFileEditV1(
                ExactFileEditV1Output::decode_with_decoder(decoder)?,
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
pub struct GuardedTextEditV1Input {
    pub kind: FileKindV1,
    pub before: ContentId,
    pub expected: ContentId,
    pub replacement: ContentId,
    pub after: ContentId,
    pub start_anchor: bool,
    pub end_anchor: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GuardedTextEditV1Output {
    pub operation_kind: u8,
    pub kind: FileKindV1,
    pub before: ContentId,
    pub expected: ContentId,
    pub replacement: ContentId,
    pub after: ContentId,
    pub start_anchor: bool,
    pub end_anchor: bool,
}

pub type GuardedTextEditV1 = GuardedTextEditV1Output;

impl GuardedTextEditV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(0);
        self.kind.encode_into(encoder)?;
        self.before.encode_into(encoder)?;
        self.expected.encode_into(encoder)?;
        self.replacement.encode_into(encoder)?;
        self.after.encode_into(encoder)?;
        encoder.write_byte(if self.start_anchor { 1 } else { 0 });
        encoder.write_byte(if self.end_anchor { 1 } else { 0 });
        Ok(())
    }
}

impl GuardedTextEditV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let operation_kind = decoder.read_byte()?;
        if operation_kind != 0u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 0, got {}",
                operation_kind
            )));
        }
        let kind = FileKindV1::decode_with_decoder(decoder)?;
        let before = ContentId::decode_with_decoder(decoder)?;
        let expected = ContentId::decode_with_decoder(decoder)?;
        let replacement = ContentId::decode_with_decoder(decoder)?;
        let after = ContentId::decode_with_decoder(decoder)?;
        let start_anchor = decoder.read_byte()? != 0;
        let end_anchor = decoder.read_byte()? != 0;
        Ok(Self {
            operation_kind,
            kind,
            before,
            expected,
            replacement,
            after,
            start_anchor,
            end_anchor,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        GuardedTextEditV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        GuardedTextEditV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<GuardedTextEditV1Output> for GuardedTextEditV1Input {
    fn from(o: GuardedTextEditV1Output) -> Self {
        Self {
            kind: o.kind,
            before: o.before,
            expected: o.expected,
            replacement: o.replacement,
            after: o.after,
            start_anchor: o.start_anchor,
            end_anchor: o.end_anchor,
        }
    }
}

impl From<GuardedTextEditV1Input> for GuardedTextEditV1Output {
    fn from(i: GuardedTextEditV1Input) -> Self {
        Self {
            operation_kind: 0u8,
            kind: i.kind,
            before: i.before,
            expected: i.expected,
            replacement: i.replacement,
            after: i.after,
            start_anchor: i.start_anchor,
            end_anchor: i.end_anchor,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExactFileEditV1Input {
    pub expected: Vec<FileValueV1>,
    pub replacement: Vec<FileValueV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExactFileEditV1Output {
    pub operation_kind: u8,
    pub expected: Vec<FileValueV1>,
    pub replacement: Vec<FileValueV1>,
}

pub type ExactFileEditV1 = ExactFileEditV1Output;

impl ExactFileEditV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(1);
        encoder.write_u32_be(self.expected.len() as u32);
        for item in &self.expected {
            item.encode_into(encoder)?;
        }
        encoder.write_u32_be(self.replacement.len() as u32);
        for item in &self.replacement {
            item.encode_into(encoder)?;
        }
        Ok(())
    }
}

impl ExactFileEditV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let operation_kind = decoder.read_byte()?;
        if operation_kind != 1u8 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                operation_kind
            )));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut expected = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FileValueV1::decode_with_decoder(decoder)?;
            expected.push(item);
        }
        let length = decoder.read_u32_be()? as usize;
        let mut replacement = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FileValueV1::decode_with_decoder(decoder)?;
            replacement.push(item);
        }
        Ok(Self {
            operation_kind,
            expected,
            replacement,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ExactFileEditV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ExactFileEditV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<ExactFileEditV1Output> for ExactFileEditV1Input {
    fn from(o: ExactFileEditV1Output) -> Self {
        Self {
            expected: o.expected,
            replacement: o.replacement,
        }
    }
}

impl From<ExactFileEditV1Input> for ExactFileEditV1Output {
    fn from(i: ExactFileEditV1Input) -> Self {
        Self {
            operation_kind: 1u8,
            expected: i.expected,
            replacement: i.replacement,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentPrefixV1Input {}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentPrefixV1Output {
    pub magic: u32,
    pub version: u16,
}

pub type SegmentPrefixV1 = SegmentPrefixV1Output;

impl SegmentPrefixV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(1464354113);
        encoder.write_u16_be(1);
        Ok(())
    }
}

impl SegmentPrefixV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let magic = decoder.read_u32_be()?;
        if magic != 1464354113u32 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1464354113, got {}",
                magic
            )));
        }
        let version = decoder.read_u16_be()?;
        if version != 1u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                version
            )));
        }
        Ok(Self { magic, version })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SegmentPrefixV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SegmentPrefixV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<SegmentPrefixV1Output> for SegmentPrefixV1Input {
    fn from(_o: SegmentPrefixV1Output) -> Self {
        Self {}
    }
}

impl From<SegmentPrefixV1Input> for SegmentPrefixV1Output {
    fn from(_i: SegmentPrefixV1Input) -> Self {
        Self {
            magic: 1464354113u32,
            version: 1u16,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentIndexEntryV1 {
    pub namespace: u8,
    pub kind: u16,
    pub digest: Digest256,
    pub offset: u64,
    pub codec: u8,
    pub stored_length: u64,
    pub logical_length: u64,
}

impl SegmentIndexEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.namespace);
        encoder.write_u16_be(self.kind);
        self.digest.encode_into(encoder)?;
        encoder.write_u64_be(self.offset);
        encoder.write_byte(self.codec);
        encoder.write_u64_be(self.stored_length);
        encoder.write_u64_be(self.logical_length);
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let namespace = decoder.read_byte()?;
        let kind = decoder.read_u16_be()?;
        let digest = Digest256::decode_with_decoder(decoder)?;
        let offset = decoder.read_u64_be()?;
        let codec = decoder.read_byte()?;
        let stored_length = decoder.read_u64_be()?;
        let logical_length = decoder.read_u64_be()?;
        Ok(Self {
            namespace,
            kind,
            digest,
            offset,
            codec,
            stored_length,
            logical_length,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentIndexPageV1 {
    pub entries: Vec<SegmentIndexEntryV1>,
}

impl SegmentIndexPageV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.entries.len() as u32);
        for item in &self.entries {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let mut entries = Vec::with_capacity(length);
        for _ in 0..length {
            let item = SegmentIndexEntryV1::decode_with_decoder(decoder)?;
            entries.push(item);
        }
        Ok(Self { entries })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentIndexKeyV1 {
    pub namespace: u8,
    pub digest: Digest256,
}

impl SegmentIndexKeyV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_byte(self.namespace);
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let namespace = decoder.read_byte()?;
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self { namespace, digest })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentDirectoryEntryV1 {
    pub first_key: SegmentIndexKeyV1,
    pub offset: u64,
    pub length: u64,
    pub page_digest: Digest256,
}

impl SegmentDirectoryEntryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.first_key.encode_into(encoder)?;
        encoder.write_u64_be(self.offset);
        encoder.write_u64_be(self.length);
        self.page_digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let first_key = SegmentIndexKeyV1::decode_with_decoder(decoder)?;
        let offset = decoder.read_u64_be()?;
        let length = decoder.read_u64_be()?;
        let page_digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            first_key,
            offset,
            length,
            page_digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentDirectoryV1 {
    pub pages: Vec<SegmentDirectoryEntryV1>,
}

impl SegmentDirectoryV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.pages.len() as u32);
        for item in &self.pages {
            item.encode_into(encoder)?;
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let length = decoder.read_u32_be()? as usize;
        let mut pages = Vec::with_capacity(length);
        for _ in 0..length {
            let item = SegmentDirectoryEntryV1::decode_with_decoder(decoder)?;
            pages.push(item);
        }
        Ok(Self { pages })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentFooterV1Input {
    pub total_length: u64,
    pub object_count: u64,
    pub index_start: u64,
    pub directory_offset: u64,
    pub directory_length: u64,
    pub body_digest: Digest256,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentFooterV1Output {
    pub magic: u32,
    pub version: u16,
    pub total_length: u64,
    pub object_count: u64,
    pub index_start: u64,
    pub directory_offset: u64,
    pub directory_length: u64,
    pub body_digest: Digest256,
}

pub type SegmentFooterV1 = SegmentFooterV1Output;

impl SegmentFooterV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(1464354388);
        encoder.write_u16_be(1);
        encoder.write_u64_be(self.total_length);
        encoder.write_u64_be(self.object_count);
        encoder.write_u64_be(self.index_start);
        encoder.write_u64_be(self.directory_offset);
        encoder.write_u64_be(self.directory_length);
        self.body_digest.encode_into(encoder)?;
        Ok(())
    }
}

impl SegmentFooterV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let magic = decoder.read_u32_be()?;
        if magic != 1464354388u32 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1464354388, got {}",
                magic
            )));
        }
        let version = decoder.read_u16_be()?;
        if version != 1u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                version
            )));
        }
        let total_length = decoder.read_u64_be()?;
        let object_count = decoder.read_u64_be()?;
        let index_start = decoder.read_u64_be()?;
        let directory_offset = decoder.read_u64_be()?;
        let directory_length = decoder.read_u64_be()?;
        let body_digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            magic,
            version,
            total_length,
            object_count,
            index_start,
            directory_offset,
            directory_length,
            body_digest,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SegmentFooterV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SegmentFooterV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<SegmentFooterV1Output> for SegmentFooterV1Input {
    fn from(o: SegmentFooterV1Output) -> Self {
        Self {
            total_length: o.total_length,
            object_count: o.object_count,
            index_start: o.index_start,
            directory_offset: o.directory_offset,
            directory_length: o.directory_length,
            body_digest: o.body_digest,
        }
    }
}

impl From<SegmentFooterV1Input> for SegmentFooterV1Output {
    fn from(i: SegmentFooterV1Input) -> Self {
        Self {
            magic: 1464354388u32,
            version: 1u16,
            total_length: i.total_length,
            object_count: i.object_count,
            index_start: i.index_start,
            directory_offset: i.directory_offset,
            directory_length: i.directory_length,
            body_digest: i.body_digest,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentCatalogRecordV1Input {
    pub generation: u64,
    pub previous: Option<Digest256>,
    pub segment: Digest256,
    pub stored_length: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentCatalogRecordV1Output {
    pub magic: u32,
    pub version: u16,
    pub generation: u64,
    pub previous: Option<Digest256>,
    pub segment: Digest256,
    pub stored_length: u64,
}

pub type SegmentCatalogRecordV1 = SegmentCatalogRecordV1Output;

impl SegmentCatalogRecordV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(1464353620);
        encoder.write_u16_be(1);
        encoder.write_u64_be(self.generation);
        if let Some(ref v) = self.previous {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        self.segment.encode_into(encoder)?;
        encoder.write_u64_be(self.stored_length);
        Ok(())
    }
}

impl SegmentCatalogRecordV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let magic = decoder.read_u32_be()?;
        if magic != 1464353620u32 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1464353620, got {}",
                magic
            )));
        }
        let version = decoder.read_u16_be()?;
        if version != 1u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!(
                "expected 1, got {}",
                version
            )));
        }
        let generation = decoder.read_u64_be()?;
        let has_value = decoder.read_uint8()? != 0;
        let previous = if has_value {
            Some(Digest256::decode_with_decoder(decoder)?)
        } else {
            None
        };
        let segment = Digest256::decode_with_decoder(decoder)?;
        let stored_length = decoder.read_u64_be()?;
        Ok(Self {
            magic,
            version,
            generation,
            previous,
            segment,
            stored_length,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        SegmentCatalogRecordV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        SegmentCatalogRecordV1Input::from(self.clone()).encode_into(encoder)
    }
}

impl From<SegmentCatalogRecordV1Output> for SegmentCatalogRecordV1Input {
    fn from(o: SegmentCatalogRecordV1Output) -> Self {
        Self {
            generation: o.generation,
            previous: o.previous,
            segment: o.segment,
            stored_length: o.stored_length,
        }
    }
}

impl From<SegmentCatalogRecordV1Input> for SegmentCatalogRecordV1Output {
    fn from(i: SegmentCatalogRecordV1Input) -> Self {
        Self {
            magic: 1464353620u32,
            version: 1u16,
            generation: i.generation,
            previous: i.previous,
            segment: i.segment,
            stored_length: i.stored_length,
        }
    }
}
