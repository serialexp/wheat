#![allow(non_camel_case_types)]
#![allow(dead_code)]
#![allow(unreachable_code)]

#[allow(unused_imports)]
use binschema_runtime::{BitStreamEncoder, BitStreamDecoder, Endianness, BitOrder, Result, BinSchemaError, EncodeContext, FieldValue};
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
    RepositoryStateV1(RepositoryStateV1Output),
    RevisionMapNodeV1(RevisionMapNodeV1Output),
    ApplicationMapNodeV1(ApplicationMapNodeV1Output),
    ConflictSetNodeV1(ConflictSetNodeV1Output),
    ContentSetNodeV1(ContentSetNodeV1Output),
    RetainedSetNodeV1(RetainedSetNodeV1Output),
}

impl WheatObject {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        match self {
            WheatObject::TreeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint32(v.entries.len() as u32, Endianness::BigEndian);
                for item in &v.entries {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ChangeV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.change_id.encode_into(encoder)?;
                v.metadata.encode_into(encoder)?;
            }
            WheatObject::RevisionV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
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
                v.transformation.encode_into(encoder)?;
                v.resulting_tree.encode_into(encoder)?;
            }
            WheatObject::ApplicationV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.revision_id.encode_into(encoder)?;
                v.target_view.encode_into(encoder)?;
                v.result.encode_into(encoder)?;
            }
            WheatObject::ConflictV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.target_view.encode_into(encoder)?;
                encoder.write_uint32(v.paths.len() as u32, Endianness::BigEndian);
                for item in &v.paths {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ViewV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.tree.encode_into(encoder)?;
                v.revisions.encode_into(encoder)?;
                v.applications.encode_into(encoder)?;
                v.unresolved_conflicts.encode_into(encoder)?;
                v.required_content.encode_into(encoder)?;
            }
            WheatObject::ContentManifestV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.content.encode_into(encoder)?;
            }
            WheatObject::RepositoryStateV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                v.base_view.encode_into(encoder)?;
                v.working_view.encode_into(encoder)?;
                v.published_view.encode_into(encoder)?;
                v.required_content.encode_into(encoder)?;
                v.retained_changes.encode_into(encoder)?;
                v.retained_views.encode_into(encoder)?;
                v.retained_revisions.encode_into(encoder)?;
                v.retained_applications.encode_into(encoder)?;
            }
            WheatObject::RevisionMapNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint8(v.depth);
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
                encoder.write_uint8(v.depth);
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
                encoder.write_uint8(v.depth);
                encoder.write_uint8(if v.present { 1 } else { 0 });
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::ContentSetNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint8(v.depth);
                encoder.write_uint8(if v.present { 1 } else { 0 });
                encoder.write_uint32(v.children.len() as u32, Endianness::BigEndian);
                for item in &v.children {
                    item.encode_into(encoder)?;
                }
            }
            WheatObject::RetainedSetNodeV1(v) => {
                encoder.write_uint16(v.wire_tag, Endianness::BigEndian);
                encoder.write_uint8(v.depth);
                encoder.write_uint8(if v.present { 1 } else { 0 });
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
            Ok(WheatObject::TreeV1(TreeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0201 {
            Ok(WheatObject::ChangeV1(ChangeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0301 {
            Ok(WheatObject::RevisionV1(RevisionV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0401 {
            Ok(WheatObject::ApplicationV1(ApplicationV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0501 {
            Ok(WheatObject::ConflictV1(ConflictV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0601 {
            Ok(WheatObject::ViewV1(ViewV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0701 {
            Ok(WheatObject::ContentManifestV1(ContentManifestV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0801 {
            Ok(WheatObject::RepositoryStateV1(RepositoryStateV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0901 {
            Ok(WheatObject::RevisionMapNodeV1(RevisionMapNodeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0a01 {
            Ok(WheatObject::ApplicationMapNodeV1(ApplicationMapNodeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0b01 {
            Ok(WheatObject::ConflictSetNodeV1(ConflictSetNodeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0c01 {
            Ok(WheatObject::ContentSetNodeV1(ContentSetNodeV1Output::decode_with_decoder(decoder)?))
        } else if value == 0x0d01 {
            Ok(WheatObject::RetainedSetNodeV1(RetainedSetNodeV1Output::decode_with_decoder(decoder)?))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("unknown discriminator value: {}", value)))
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
        let mut bytes = Vec::with_capacity(16);
        for _ in 0..16 {
            let item = decoder.read_byte()?;
            bytes.push(item);
        }
        Ok(Self {
            bytes,
        })
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
        let mut bytes = Vec::with_capacity(32);
        for _ in 0..32 {
            let item = decoder.read_byte()?;
            bytes.push(item);
        }
        Ok(Self {
            bytes,
        })
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("algorithm".to_string(), FieldValue::U8(self.algorithm));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.algorithm);
        // Encode nested struct digest
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
        Ok(Self {
            algorithm,
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeId {
    pub uuid: Uuid128,
}

impl ChangeId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct uuid
        self.uuid.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let uuid = Uuid128::decode_with_decoder(decoder)?;
        Ok(Self {
            uuid,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionId {
    pub digest: Digest256,
}

impl RevisionId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeId {
    pub digest: Digest256,
}

impl TreeId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeObjectId {
    pub digest: Digest256,
}

impl ChangeObjectId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationId {
    pub digest: Digest256,
}

impl ApplicationId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictId {
    pub digest: Digest256,
}

impl ConflictId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewId {
    pub digest: Digest256,
}

impl ViewId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentManifestId {
    pub digest: Digest256,
}

impl ContentManifestId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapId {
    pub digest: Digest256,
}

impl RevisionMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapId {
    pub digest: Digest256,
}

impl ApplicationMapId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetId {
    pub digest: Digest256,
}

impl ConflictSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetId {
    pub digest: Digest256,
}

impl ContentSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetId {
    pub digest: Digest256,
}

impl RetainedSetId {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        // Encode nested struct digest
        self.digest.encode_into(encoder)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let digest = Digest256::decode_with_decoder(decoder)?;
        Ok(Self {
            digest,
        })
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 257, got {}", wire_tag)));
        }
        let length = decoder.read_u32_be()? as usize;
        let mut entries = Vec::with_capacity(length);
        for _ in 0..length {
            let item = DirectoryEntryV1::decode_with_decoder(decoder)?;
            entries.push(item);
        }
        Ok(Self {
            wire_tag,
            entries,
        })
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
        Self {
            entries: o.entries,
        }
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
        let name = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let value = DirectoryEntryValueV1::decode_with_decoder(decoder)?;
        Ok(Self {
            name,
            value,
        })
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        match self {
            DirectoryEntryValueV1::FileEntryV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint8(v.entry_kind);
                v.content.encode_into(encoder)?;
            }
            DirectoryEntryValueV1::DirectoryEntryRefV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
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
            Ok(DirectoryEntryValueV1::FileEntryV1(FileEntryV1Output::decode_with_decoder(decoder)?))
        } else if value == 1 {
            Ok(DirectoryEntryValueV1::DirectoryEntryRefV1(DirectoryEntryRefV1Output::decode_with_decoder(decoder)?))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("unknown discriminator value: {}", value)))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntryV1Input {
    pub content: ContentId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntryV1Output {
    pub entry_kind: u8,
    pub content: ContentId,
}

pub type FileEntryV1 = FileEntryV1Output;

impl FileEntryV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(0);
        // Encode nested struct content with context
        self.content.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 0, got {}", entry_kind)));
        }
        let content = ContentId::decode_with_decoder(decoder)?;
        Ok(Self {
            entry_kind,
            content,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        FileEntryV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        FileEntryV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        FileEntryV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        FileEntryV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<FileEntryV1Output> for FileEntryV1Input {
    fn from(o: FileEntryV1Output) -> Self {
        Self {
            content: o.content,
        }
    }
}

impl From<FileEntryV1Input> for FileEntryV1Output {
    fn from(i: FileEntryV1Input) -> Self {
        Self {
            entry_kind: 0u8,
            content: i.content,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(1);
        // Encode nested struct directory with context
        self.directory.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1, got {}", entry_kind)));
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        DirectoryEntryRefV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        DirectoryEntryRefV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
        let name = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let email = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        Ok(Self {
            name,
            email,
        })
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
        let system = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
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
        Ok(Self {
            bytes,
        })
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("title".to_string(), FieldValue::String(self.title.clone()));
        parent_fields.insert("description".to_string(), FieldValue::String(self.description.clone()));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
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
        // Encode nested struct creator
        self.creator.encode_into(encoder)?;
        // Encode nested struct created_at
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
        let title = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let length = decoder.read_u32_be()? as usize;
        let bytes = decoder.read_bytes_vec(length)?;
        let description = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("message".to_string(), FieldValue::String(self.message.clone()));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u32_be(self.message.len() as u32);
        let string_bytes: &[u8] = self.message.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        // Encode nested struct author
        self.author.encode_into(encoder)?;
        // Encode nested struct authored_at
        self.authored_at.encode_into(encoder)?;
        // Encode nested struct committer
        self.committer.encode_into(encoder)?;
        // Encode nested struct committed_at
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
        let message = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(513);
        // Encode nested struct change_id with context
        self.change_id.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct metadata with context
        self.metadata.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 513, got {}", wire_tag)));
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ChangeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ChangeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
    pub transformation: TransformationV1,
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
    pub transformation: TransformationV1,
    pub resulting_tree: TreeId,
}

pub type RevisionV1 = RevisionV1Output;

impl RevisionV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        // Collect items with sub-field values for typed array 'predecessors'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.predecessors {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("RevisionId".to_string(), item_fields));
            }
            parent_fields.insert("predecessors".to_string(), FieldValue::Items(items_data));
        }
        // Collect items with sub-field values for typed array 'dependencies'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.dependencies {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("ChangeId".to_string(), item_fields));
            }
            parent_fields.insert("dependencies".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(769);
        // Encode nested struct change_id with context
        self.change_id.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct metadata with context
        self.metadata.encode_into_with_context(encoder, &child_ctx)?;
        encoder.write_u32_be(self.predecessors.len() as u32);
        for item in &self.predecessors {
            item.encode_into_with_context(encoder, &child_ctx)?;
        }
        encoder.write_u32_be(self.dependencies.len() as u32);
        for item in &self.dependencies {
            item.encode_into_with_context(encoder, &child_ctx)?;
        }
        // Encode nested struct authored_against with context
        self.authored_against.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct transformation
        self.transformation.encode_into(encoder)?;
        // Encode nested struct resulting_tree with context
        self.resulting_tree.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 769, got {}", wire_tag)));
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
        let transformation = TransformationV1::decode_with_decoder(decoder)?;
        let resulting_tree = TreeId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            change_id,
            metadata,
            predecessors,
            dependencies,
            authored_against,
            transformation,
            resulting_tree,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RevisionV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RevisionV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        RevisionV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        RevisionV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
            transformation: o.transformation,
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
            transformation: i.transformation,
            resulting_tree: i.resulting_tree,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransformationV1 {
    pub edits: Vec<FileEditV1>,
}

impl TransformationV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.edits.len() as u32);
        for item in &self.edits {
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
        let mut edits = Vec::with_capacity(length);
        for _ in 0..length {
            let item = FileEditV1::decode_with_decoder(decoder)?;
            edits.push(item);
        }
        Ok(Self {
            edits,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEditV1 {
    pub path: std::string::String,
    pub expected: Option<ContentId>,
    pub replacement: Option<ContentId>,
}

impl FileEditV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.path.len() as u32);
        let string_bytes: &[u8] = self.path.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        if let Some(ref v) = self.expected {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        if let Some(ref v) = self.replacement {
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
        let path = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let has_value = decoder.read_uint8()? != 0;
        let expected = if has_value {
            Some(ContentId::decode_with_decoder(decoder)?)
        } else {
            None
        };
        let has_value = decoder.read_uint8()? != 0;
        let replacement = if has_value {
            Some(ContentId::decode_with_decoder(decoder)?)
        } else {
            None
        };
        Ok(Self {
            path,
            expected,
            replacement,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationV1Input {
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub result: ApplicationResultV1,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationV1Output {
    pub wire_tag: u16,
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub result: ApplicationResultV1,
}

pub type ApplicationV1 = ApplicationV1Output;

impl ApplicationV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(1025);
        // Encode nested struct revision_id with context
        self.revision_id.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct target_view with context
        self.target_view.encode_into_with_context(encoder, &child_ctx)?;
        self.result.encode_into(encoder)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1025, got {}", wire_tag)));
        }
        let revision_id = RevisionId::decode_with_decoder(decoder)?;
        let target_view = ViewId::decode_with_decoder(decoder)?;
        let result = ApplicationResultV1::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            revision_id,
            target_view,
            result,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ApplicationV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ApplicationV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ApplicationV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ApplicationV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ApplicationV1Output> for ApplicationV1Input {
    fn from(o: ApplicationV1Output) -> Self {
        Self {
            revision_id: o.revision_id,
            target_view: o.target_view,
            result: o.result,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        match self {
            ApplicationResultV1::CleanApplicationV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
                encoder.write_uint8(v.result_kind);
                v.resulting_tree.encode_into(encoder)?;
            }
            ApplicationResultV1::ConflictedApplicationV1(v) => {
                // Build per-variant context for nested struct sub-fields
                let variant_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                let _variant_ctx = ctx.extend_with_parent(variant_fields);
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
            Ok(ApplicationResultV1::CleanApplicationV1(CleanApplicationV1Output::decode_with_decoder(decoder)?))
        } else if value == 1 {
            Ok(ApplicationResultV1::ConflictedApplicationV1(ConflictedApplicationV1Output::decode_with_decoder(decoder)?))
        } else {
            Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("unknown discriminator value: {}", value)))
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(0);
        // Encode nested struct resulting_tree with context
        self.resulting_tree.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 0, got {}", result_kind)));
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        CleanApplicationV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        CleanApplicationV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(1);
        // Encode nested struct partial_tree with context
        self.partial_tree.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct conflict with context
        self.conflict.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1, got {}", result_kind)));
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ConflictedApplicationV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ConflictedApplicationV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
    pub paths: Vec<PathConflictV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictV1Output {
    pub wire_tag: u16,
    pub target_view: ViewId,
    pub paths: Vec<PathConflictV1>,
}

pub type ConflictV1 = ConflictV1Output;

impl ConflictV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        // Collect items with sub-field values for typed array 'paths'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for item in &self.paths {
                let item_bytes = item.encode()?;
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                item_fields.insert("path".to_string(), FieldValue::String(item.path.clone()));
                items_data.push(("PathConflictV1".to_string(), item_fields));
            }
            parent_fields.insert("paths".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(1281);
        // Encode nested struct target_view with context
        self.target_view.encode_into_with_context(encoder, &child_ctx)?;
        encoder.write_u32_be(self.paths.len() as u32);
        for item in &self.paths {
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1281, got {}", wire_tag)));
        }
        let target_view = ViewId::decode_with_decoder(decoder)?;
        let length = decoder.read_u32_be()? as usize;
        let mut paths = Vec::with_capacity(length);
        for _ in 0..length {
            let item = PathConflictV1::decode_with_decoder(decoder)?;
            paths.push(item);
        }
        Ok(Self {
            wire_tag,
            target_view,
            paths,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ConflictV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ConflictV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ConflictV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ConflictV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ConflictV1Output> for ConflictV1Input {
    fn from(o: ConflictV1Output) -> Self {
        Self {
            target_view: o.target_view,
            paths: o.paths,
        }
    }
}

impl From<ConflictV1Input> for ConflictV1Output {
    fn from(i: ConflictV1Input) -> Self {
        Self {
            wire_tag: 1281u16,
            target_view: i.target_view,
            paths: i.paths,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathConflictV1 {
    pub path: std::string::String,
    pub expected: Option<ContentId>,
    pub actual: Option<ContentId>,
    pub proposed: Option<ContentId>,
}

impl PathConflictV1 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into(&mut encoder)?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        encoder.write_u32_be(self.path.len() as u32);
        let string_bytes: &[u8] = self.path.as_bytes();
        for &b in string_bytes.iter() {
            encoder.write_byte(b);
        }
        if let Some(ref v) = self.expected {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        if let Some(ref v) = self.actual {
            encoder.write_uint8(1);
            v.encode_into(encoder)?;
        } else {
            encoder.write_uint8(0);
        }
        if let Some(ref v) = self.proposed {
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
        let path = std::string::String::from_utf8(bytes).map_err(|_| binschema_runtime::BinSchemaError::InvalidUtf8)?;
        let has_value = decoder.read_uint8()? != 0;
        let expected = if has_value {
            Some(ContentId::decode_with_decoder(decoder)?)
        } else {
            None
        };
        let has_value = decoder.read_uint8()? != 0;
        let actual = if has_value {
            Some(ContentId::decode_with_decoder(decoder)?)
        } else {
            None
        };
        let has_value = decoder.read_uint8()? != 0;
        let proposed = if has_value {
            Some(ContentId::decode_with_decoder(decoder)?)
        } else {
            None
        };
        Ok(Self {
            path,
            expected,
            actual,
            proposed,
        })
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(1537);
        // Encode nested struct tree with context
        self.tree.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct revisions with context
        self.revisions.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct applications with context
        self.applications.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct unresolved_conflicts with context
        self.unresolved_conflicts.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct required_content with context
        self.required_content.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1537, got {}", wire_tag)));
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ViewV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ViewV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(1793);
        // Encode nested struct content with context
        self.content.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 1793, got {}", wire_tag)));
        }
        let content = ContentSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            content,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ContentManifestV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        ContentManifestV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ContentManifestV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ContentManifestV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ContentManifestV1Output> for ContentManifestV1Input {
    fn from(o: ContentManifestV1Output) -> Self {
        Self {
            content: o.content,
        }
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
    pub depth: u8,
    pub value: Vec<RevisionId>,
    pub children: Vec<RevisionMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevisionMapNodeV1Output {
    pub wire_tag: u16,
    pub depth: u8,
    pub value: Vec<RevisionId>,
    pub children: Vec<RevisionMapChildV1>,
}

pub type RevisionMapNodeV1 = RevisionMapNodeV1Output;

impl RevisionMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("depth".to_string(), FieldValue::U8(self.depth));
        // Collect items with sub-field values for typed array 'value'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.value {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("RevisionId".to_string(), item_fields));
            }
            parent_fields.insert("value".to_string(), FieldValue::Items(items_data));
        }
        // Collect items with sub-field values for typed array 'children'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.children {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("RevisionMapChildV1".to_string(), item_fields));
            }
            parent_fields.insert("children".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(2305);
        encoder.write_byte(self.depth);
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into_with_context(encoder, &child_ctx)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 2305, got {}", wire_tag)));
        }
        let depth = decoder.read_byte()?;
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
            depth,
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        RevisionMapNodeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        RevisionMapNodeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<RevisionMapNodeV1Output> for RevisionMapNodeV1Input {
    fn from(o: RevisionMapNodeV1Output) -> Self {
        Self {
            depth: o.depth,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<RevisionMapNodeV1Input> for RevisionMapNodeV1Output {
    fn from(i: RevisionMapNodeV1Input) -> Self {
        Self {
            wire_tag: 2305u16,
            depth: i.depth,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("edge".to_string(), FieldValue::U8(self.edge));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.edge);
        // Encode nested struct child with context
        self.child.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = RevisionMapId::decode_with_decoder(decoder)?;
        Ok(Self {
            edge,
            child,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapNodeV1Input {
    pub depth: u8,
    pub value: Vec<ApplicationId>,
    pub children: Vec<ApplicationMapChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplicationMapNodeV1Output {
    pub wire_tag: u16,
    pub depth: u8,
    pub value: Vec<ApplicationId>,
    pub children: Vec<ApplicationMapChildV1>,
}

pub type ApplicationMapNodeV1 = ApplicationMapNodeV1Output;

impl ApplicationMapNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("depth".to_string(), FieldValue::U8(self.depth));
        // Collect items with sub-field values for typed array 'value'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.value {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("ApplicationId".to_string(), item_fields));
            }
            parent_fields.insert("value".to_string(), FieldValue::Items(items_data));
        }
        // Collect items with sub-field values for typed array 'children'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.children {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("ApplicationMapChildV1".to_string(), item_fields));
            }
            parent_fields.insert("children".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(2561);
        encoder.write_byte(self.depth);
        encoder.write_u32_be(self.value.len() as u32);
        for item in &self.value {
            item.encode_into_with_context(encoder, &child_ctx)?;
        }
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 2561, got {}", wire_tag)));
        }
        let depth = decoder.read_byte()?;
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
            depth,
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ApplicationMapNodeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ApplicationMapNodeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ApplicationMapNodeV1Output> for ApplicationMapNodeV1Input {
    fn from(o: ApplicationMapNodeV1Output) -> Self {
        Self {
            depth: o.depth,
            value: o.value,
            children: o.children,
        }
    }
}

impl From<ApplicationMapNodeV1Input> for ApplicationMapNodeV1Output {
    fn from(i: ApplicationMapNodeV1Input) -> Self {
        Self {
            wire_tag: 2561u16,
            depth: i.depth,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("edge".to_string(), FieldValue::U8(self.edge));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.edge);
        // Encode nested struct child with context
        self.child.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ApplicationMapId::decode_with_decoder(decoder)?;
        Ok(Self {
            edge,
            child,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetNodeV1Input {
    pub depth: u8,
    pub present: bool,
    pub children: Vec<ConflictSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictSetNodeV1Output {
    pub wire_tag: u16,
    pub depth: u8,
    pub present: bool,
    pub children: Vec<ConflictSetChildV1>,
}

pub type ConflictSetNodeV1 = ConflictSetNodeV1Output;

impl ConflictSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("depth".to_string(), FieldValue::U8(self.depth));
        // Collect items with sub-field values for typed array 'children'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.children {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("ConflictSetChildV1".to_string(), item_fields));
            }
            parent_fields.insert("children".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(2817);
        encoder.write_byte(self.depth);
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 2817, got {}", wire_tag)));
        }
        let depth = decoder.read_byte()?;
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ConflictSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            depth,
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ConflictSetNodeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ConflictSetNodeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ConflictSetNodeV1Output> for ConflictSetNodeV1Input {
    fn from(o: ConflictSetNodeV1Output) -> Self {
        Self {
            depth: o.depth,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<ConflictSetNodeV1Input> for ConflictSetNodeV1Output {
    fn from(i: ConflictSetNodeV1Input) -> Self {
        Self {
            wire_tag: 2817u16,
            depth: i.depth,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("edge".to_string(), FieldValue::U8(self.edge));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.edge);
        // Encode nested struct child with context
        self.child.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ConflictSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            edge,
            child,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetNodeV1Input {
    pub depth: u8,
    pub present: bool,
    pub children: Vec<ContentSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentSetNodeV1Output {
    pub wire_tag: u16,
    pub depth: u8,
    pub present: bool,
    pub children: Vec<ContentSetChildV1>,
}

pub type ContentSetNodeV1 = ContentSetNodeV1Output;

impl ContentSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("depth".to_string(), FieldValue::U8(self.depth));
        // Collect items with sub-field values for typed array 'children'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.children {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("ContentSetChildV1".to_string(), item_fields));
            }
            parent_fields.insert("children".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(3073);
        encoder.write_byte(self.depth);
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 3073, got {}", wire_tag)));
        }
        let depth = decoder.read_byte()?;
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = ContentSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            depth,
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        ContentSetNodeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        ContentSetNodeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<ContentSetNodeV1Output> for ContentSetNodeV1Input {
    fn from(o: ContentSetNodeV1Output) -> Self {
        Self {
            depth: o.depth,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<ContentSetNodeV1Input> for ContentSetNodeV1Output {
    fn from(i: ContentSetNodeV1Input) -> Self {
        Self {
            wire_tag: 3073u16,
            depth: i.depth,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("edge".to_string(), FieldValue::U8(self.edge));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.edge);
        // Encode nested struct child with context
        self.child.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = ContentSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            edge,
            child,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetNodeV1Input {
    pub depth: u8,
    pub present: bool,
    pub children: Vec<RetainedSetChildV1>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetainedSetNodeV1Output {
    pub wire_tag: u16,
    pub depth: u8,
    pub present: bool,
    pub children: Vec<RetainedSetChildV1>,
}

pub type RetainedSetNodeV1 = RetainedSetNodeV1Output;

impl RetainedSetNodeV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("depth".to_string(), FieldValue::U8(self.depth));
        // Collect items with sub-field values for typed array 'children'
        {
            let mut items_data: Vec<(std::string::String, HashMap<std::string::String, FieldValue>)> = Vec::new();
            for _ in &self.children {
                let item_bytes = Vec::<u8>::new(); // Items need context, skip encoding for now
                let mut item_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
                item_fields.insert("_encoded_size".to_string(), FieldValue::U64(item_bytes.len() as u64));
                items_data.push(("RetainedSetChildV1".to_string(), item_fields));
            }
            parent_fields.insert("children".to_string(), FieldValue::Items(items_data));
        }
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(3329);
        encoder.write_byte(self.depth);
        encoder.write_byte(if self.present { 1 } else { 0 });
        encoder.write_u32_be(self.children.len() as u32);
        for item in &self.children {
            item.encode_into_with_context(encoder, &child_ctx)?;
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
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 3329, got {}", wire_tag)));
        }
        let depth = decoder.read_byte()?;
        let present = decoder.read_byte()? != 0;
        let length = decoder.read_u32_be()? as usize;
        let mut children = Vec::with_capacity(length);
        for _ in 0..length {
            let item = RetainedSetChildV1::decode_with_decoder(decoder)?;
            children.push(item);
        }
        Ok(Self {
            wire_tag,
            depth,
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
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        RetainedSetNodeV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        RetainedSetNodeV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<RetainedSetNodeV1Output> for RetainedSetNodeV1Input {
    fn from(o: RetainedSetNodeV1Output) -> Self {
        Self {
            depth: o.depth,
            present: o.present,
            children: o.children,
        }
    }
}

impl From<RetainedSetNodeV1Input> for RetainedSetNodeV1Output {
    fn from(i: RetainedSetNodeV1Input) -> Self {
        Self {
            wire_tag: 3329u16,
            depth: i.depth,
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
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let mut parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        parent_fields.insert("edge".to_string(), FieldValue::U8(self.edge));
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_byte(self.edge);
        // Encode nested struct child with context
        self.child.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let edge = decoder.read_byte()?;
        let child = RetainedSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            edge,
            child,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateV1Input {
    pub base_view: ViewId,
    pub working_view: ViewId,
    pub published_view: ViewId,
    pub required_content: ContentManifestId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStateV1Output {
    pub wire_tag: u16,
    pub base_view: ViewId,
    pub working_view: ViewId,
    pub published_view: ViewId,
    pub required_content: ContentManifestId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
}

pub type RepositoryStateV1 = RepositoryStateV1Output;

impl RepositoryStateV1Input {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, &EncodeContext::new())?;
        Ok(encoder.finish())
    }

    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        self.encode_into_with_context(encoder, &EncodeContext::new())
    }

    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        let mut encoder = BitStreamEncoder::new(BitOrder::MsbFirst);
        self.encode_into_with_context(&mut encoder, ctx)?;
        Ok(encoder.finish())
    }

    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {

        // Build parent context for nested struct encoding
        let parent_fields: HashMap<std::string::String, FieldValue> = HashMap::new();
        let child_ctx = ctx.extend_with_parent(parent_fields);
        let _ = &child_ctx; // Used by nested struct encoding
        encoder.write_u16_be(2049);
        // Encode nested struct base_view with context
        self.base_view.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct working_view with context
        self.working_view.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct published_view with context
        self.published_view.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct required_content with context
        self.required_content.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct retained_changes with context
        self.retained_changes.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct retained_views with context
        self.retained_views.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct retained_revisions with context
        self.retained_revisions.encode_into_with_context(encoder, &child_ctx)?;
        // Encode nested struct retained_applications with context
        self.retained_applications.encode_into_with_context(encoder, &child_ctx)?;
        Ok(())
    }

}

impl RepositoryStateV1Output {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder = BitStreamDecoder::new(bytes, BitOrder::MsbFirst);
        Self::decode_with_decoder(&mut decoder)
    }

    pub fn decode_with_decoder(decoder: &mut BitStreamDecoder) -> Result<Self> {
        let wire_tag = decoder.read_u16_be()?;
        if wire_tag != 2049u16 {
            return Err(binschema_runtime::BinSchemaError::InvalidVariant(format!("expected 2049, got {}", wire_tag)));
        }
        let base_view = ViewId::decode_with_decoder(decoder)?;
        let working_view = ViewId::decode_with_decoder(decoder)?;
        let published_view = ViewId::decode_with_decoder(decoder)?;
        let required_content = ContentManifestId::decode_with_decoder(decoder)?;
        let retained_changes = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_views = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_revisions = RetainedSetId::decode_with_decoder(decoder)?;
        let retained_applications = RetainedSetId::decode_with_decoder(decoder)?;
        Ok(Self {
            wire_tag,
            base_view,
            working_view,
            published_view,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        RepositoryStateV1Input::from(self.clone()).encode()
    }
    pub fn encode_into(&self, encoder: &mut BitStreamEncoder) -> Result<()> {
        RepositoryStateV1Input::from(self.clone()).encode_into(encoder)
    }
    pub fn encode_with_context(&self, ctx: &EncodeContext) -> Result<Vec<u8>> {
        RepositoryStateV1Input::from(self.clone()).encode_with_context(ctx)
    }
    pub fn encode_into_with_context(&self, encoder: &mut BitStreamEncoder, ctx: &EncodeContext) -> Result<()> {
        RepositoryStateV1Input::from(self.clone()).encode_into_with_context(encoder, ctx)
    }
}

impl From<RepositoryStateV1Output> for RepositoryStateV1Input {
    fn from(o: RepositoryStateV1Output) -> Self {
        Self {
            base_view: o.base_view,
            working_view: o.working_view,
            published_view: o.published_view,
            required_content: o.required_content,
            retained_changes: o.retained_changes,
            retained_views: o.retained_views,
            retained_revisions: o.retained_revisions,
            retained_applications: o.retained_applications,
        }
    }
}

impl From<RepositoryStateV1Input> for RepositoryStateV1Output {
    fn from(i: RepositoryStateV1Input) -> Self {
        Self {
            wire_tag: 2049u16,
            base_view: i.base_view,
            working_view: i.working_view,
            published_view: i.published_view,
            required_content: i.required_content,
            retained_changes: i.retained_changes,
            retained_views: i.retained_views,
            retained_revisions: i.retained_revisions,
            retained_applications: i.retained_applications,
        }
    }
}
