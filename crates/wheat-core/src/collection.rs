use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, ApplicationMapId, CanonicalObjectError, ConflictSetId, ContentSetId,
    RetainedSetId,
};

macro_rules! child {
    ($name:ident, $id:ty) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct $name {
            pub edge: u8,
            pub child: $id,
        }
    };
}

child!(ApplicationMapChild, ApplicationMapId);
child!(ConflictSetChild, ConflictSetId);
child!(ContentSetChild, ContentSetId);
child!(RetainedSetChild, RetainedSetId);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationMapNode {
    pub depth: u8,
    pub value: Option<ApplicationId>,
    pub children: Vec<ApplicationMapChild>,
}

impl ApplicationMapNode {
    pub fn new(
        depth: u8,
        value: Option<ApplicationId>,
        mut children: Vec<ApplicationMapChild>,
    ) -> Result<Self, CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        let node = Self {
            depth,
            value,
            children,
        };
        crate::wire::validate_application_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::application_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ApplicationMapId {
        ApplicationMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

macro_rules! set_node {
    ($node:ident, $child:ident, $id:ty, $validate:ident, $bytes:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct $node {
            pub depth: u8,
            pub present: bool,
            pub children: Vec<$child>,
        }

        impl $node {
            pub fn new(
                depth: u8,
                present: bool,
                mut children: Vec<$child>,
            ) -> Result<Self, CanonicalObjectError> {
                children.sort_by_key(|child| child.edge);
                let node = Self {
                    depth,
                    present,
                    children,
                };
                crate::wire::$validate(&node)?;
                Ok(node)
            }

            #[must_use]
            pub fn canonical_bytes(&self) -> Vec<u8> {
                crate::wire::$bytes(self)
            }

            #[must_use]
            pub fn id(&self) -> $id {
                <$id>::of_canonical_bytes(&self.canonical_bytes())
            }
        }
    };
}

set_node!(
    ConflictSetNode,
    ConflictSetChild,
    ConflictSetId,
    validate_conflict_set_node,
    conflict_set_node_bytes
);
set_node!(
    ContentSetNode,
    ContentSetChild,
    ContentSetId,
    validate_content_set_node,
    content_set_node_bytes
);
set_node!(
    RetainedSetNode,
    RetainedSetChild,
    RetainedSetId,
    validate_retained_set_node,
    retained_set_node_bytes
);
