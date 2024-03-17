use std::{collections::HashMap, ops::Range};

use super::{LanguageLayer, LayerId};

use slotmap::HopSlotMap;
use tree_sitter::Node;

pub struct TreeCursor<'a> {
    layers: &'a HopSlotMap<LayerId, LanguageLayer>,
    root: LayerId,
    current: LayerId,
    injection_ranges: HashMap<Range<usize>, LayerId>,
    // TODO: Ideally this would be a `tree_sitter::TreeCursor<'a>` but
    // that returns very surprising results in testing.
    cursor: Node<'a>,
}

impl<'a> TreeCursor<'a> {
    pub(super) fn new(
        layers: &'a HopSlotMap<LayerId, LanguageLayer>,
        root: LayerId,
        injection_ranges: HashMap<Range<usize>, LayerId>,
    ) -> Self {
        let cursor = layers[root].tree().root_node();

        Self {
            layers,
            root,
            current: root,
            injection_ranges,
            cursor,
        }
    }

    pub fn node(&self) -> Node<'a> {
        self.cursor
    }

    pub fn goto_parent(&mut self) -> bool {
        if let Some(parent) = self.node().parent() {
            self.cursor = parent;
            return true;
        }

        // If we are already on the root layer, we cannot ascend.
        if self.current == self.root {
            return false;
        }

        // Ascend to the parent layer.
        let range = self.node().byte_range();
        let parent_id = self.layers[self.current]
            .parent
            .expect("non-root layers have a parent");
        self.current = parent_id;
        let root = self.layers[self.current].tree().root_node();
        self.cursor = root
            .descendant_for_byte_range(range.start, range.end)
            .unwrap_or(root);

        true
    }

    pub fn goto_first_child(&mut self) -> bool {
        // Check if the current node's range is an injection layer range.
        let range = self.node().byte_range();
        if let Some(layer_id) = self.injection_ranges.get(&range) {
            // Switch to the child layer.
            self.current = *layer_id;
            self.cursor = self.layers[self.current].tree().root_node();
            true
        } else if let Some(child) = self.cursor.child(0) {
            // Otherwise descend in the current tree.
            self.cursor = child;
            true
        } else {
            false
        }
    }

    pub fn goto_next_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.next_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    pub fn goto_prev_sibling(&mut self) -> bool {
        if let Some(sibling) = self.cursor.prev_sibling() {
            self.cursor = sibling;
            true
        } else {
            false
        }
    }

    pub fn reset_to_byte_range(&mut self, start: usize, end: usize) {
        let mut container_id = self.root;

        for (layer_id, layer) in self.layers.iter() {
            if layer.depth > self.layers[container_id].depth
                && layer.contains_byte_range(start, end)
            {
                container_id = layer_id;
            }
        }

        self.current = container_id;
        let root = self.layers[self.current].tree().root_node();
        self.cursor = root.descendant_for_byte_range(start, end).unwrap_or(root);
    }
}
