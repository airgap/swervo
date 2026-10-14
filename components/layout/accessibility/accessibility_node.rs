/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::LazyLock;

use accesskit::{Action, Affine, NodeId, Role};
use app_units::Au;
use bitflags::bitflags;
use euclid::Rect;
use layout_api::{
    ARIA_ROLE_TO_ACCESSKIT_ROLE, AccessibilityDamage, BoxAreaType, LayoutElement, LayoutNode,
    LayoutNodeType, SUPPORTED_ARIA_ROLES,
};
use log::trace;
use num_traits::ToPrimitive;
use rustc_hash::{FxHashMap, FxHashSet};
use script::layout_dom::{ServoLayoutElement, ServoLayoutNode};
use servo_base::print_tree::PrintTree;
use servo_config::pref;
use style::dom::{NodeInfo, OpaqueNode};
use style_traits::CSSPixel;
use web_atoms::{LocalName, local_name, ns};
use webrender_api::units::LayoutVector2D;

use super::accessibility_tree::{AccessibilityTree, AccessibilityUpdate, TreeChange};
use crate::ArcRefCell;
use crate::accessibility::AccessibilityContext;
use crate::cell::WeakRefCell;
use crate::query::{BoxAreaInclusion, process_box_area_request};

/// Convert a rectangle as layout reports it into the one [`accesskit`] wants.
fn au_rect_to_accesskit_rect(rect: Rect<Au, CSSPixel>) -> accesskit::Rect {
    accesskit::Rect::new(
        rect.min_x().to_f64_px(),
        rect.min_y().to_f64_px(),
        rect.max_x().to_f64_px(),
        rect.max_y().to_f64_px(),
    )
}

fn scroll_offset_to_affine(layout_vector: LayoutVector2D) -> Affine {
    Affine::translate((
        -layout_vector.x.to_f64().unwrap_or(0.),
        -layout_vector.y.to_f64().unwrap_or(0.),
    ))
}

bitflags! {
    /// Flags tracking an [`AccessibilityNode`]'s dirty state during an update. All flags which are
    /// set during the update should be unset by the end of the update.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) struct DirtyState : u16 {
        /// At least one descendant of this node has unresolved damage from the DOM tree.
        const DescendantHasDamage = 0b0001;
        /// This node has unresolved damage from the DOM tree.
        const HasDamage = 0b0010;
        /// This node's data changed, but it hasn't yet been added to the [`AccessibilityUpdate`].
        const Updated = 0b0100;
    }
}

bitflags! {
    /// Damage which was caused by changes to the accessibility tree. These changes can cause other
    /// properties to need to be re-computed based on the updated values, either on the same node or
    /// on other nodes.
    #[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
    pub(super) struct LocalAccessibilityDamage: u16 {
        /// This node's children changed, and/or any node in its subtree changed.
        const SubtreeChanged = 0b0001;
        /// This node's computed role changed.
        const RoleChanged = 0b0010;
        /// This node's computed label or text value (for a text node) changed.
        const TextChanged = 0b0100;
        /// This node's visibility changed.
        const VisibilityChanged = 0b1000;
        /// One of this node's author-supplied accessible name sources (`aria-labelledby`,
        /// `aria-label`, `alt` or `title`) changed, so its accessible name must be recomputed.
        const NameSourceChanged = 0b1_0000;
    }
}

pub(super) struct AccessibilityNode {
    /// The unique ID for the node. This is used both as a key in [`AccessibilityTree`]'s cache of
    /// nodes, and as an identifier in [`accesskit`] datastructures: [`accesskit::Node`]s,
    /// [`accesskit::TreeUpdate`]s and [`accesskit::ActionRequest`]s.
    id: NodeId,
    /// The computed [`accesskit::Node`] data. This will be copied and serialized into a
    /// [`accesskit::TreeUpdate`] whenever it is changed during an update.
    accesskit_node: accesskit::Node,
    /// This node's parent, if any.
    parent_node: Option<WeakRefCell<AccessibilityNode>>,
    /// All this node's children.
    child_nodes: Vec<ArcRefCell<AccessibilityNode>>,
    /// The [`OpaqueNode`] for the DOM node which corresponds to this accessibility node, if any.
    /// An accessibility node may not correspond to a DOM node if it corresponds to a
    /// pseudo-element, or in a test.
    opaque_node: Option<OpaqueNode>,
    /// This node's scroll offset, if it is a scroll container which has scrolled. This is used to
    /// translate this node's children.
    scroll_offset: Option<LayoutVector2D>,
    /// Any dirty state for the current update.
    dirty_state: DirtyState,
    /// The explicit accessible name from attributes (`aria-label`, `alt` or `title`), if any.
    /// When present, name-from-contents must not override it.
    explicit_name: Option<Box<str>>,
    /// The raw `aria-labelledby` idref list, if any. Resolved to a name in a post-pass once the
    /// tree (and its id map) has been updated, since idrefs may point forward (LYK-1378).
    labelledby: Option<Box<str>>,
    /// The accessible name resolved from [`Self::labelledby`], if it resolved to non-empty text.
    /// This is the highest-precedence name source.
    labelledby_name: Option<Box<str>>,
    /// The element's `id` attribute, as registered in the tree's id map so that
    /// `aria-labelledby` idrefs can resolve to this node.
    element_id: Option<Box<str>>,
}

impl AccessibilityNode {
    pub(super) fn new(id: NodeId) -> Self {
        Self::new_with_role(id, Role::Unknown)
    }

    pub(super) fn new_with_role(id: NodeId, role: Role) -> Self {
        Self {
            id,
            accesskit_node: accesskit::Node::new(role),
            parent_node: None,
            child_nodes: vec![],
            opaque_node: None,
            scroll_offset: None,
            dirty_state: DirtyState::empty(),
            explicit_name: None,
            labelledby: None,
            labelledby_name: None,
            element_id: None,
        }
    }

    /// Update this node and its subtree based on damage from the DOM.
    ///
    /// - First, if this node has damage from the DOM to be resolved, update the node from the DOM
    ///   tree, recursively populating any new children.
    /// - Next, recursively call this method for any children which are dirty, or have dirty
    ///   descendants.
    /// - Finally, update any properties on this node which are may have changed due to other
    ///   changes in the tree.
    ///
    /// At the end of this method, both `has_dirty_descendants` and `is_dirty` should be false for
    /// this node and all its descendants.
    pub(super) fn update_subtree<'update>(
        &mut self,
        ref_self: ArcRefCell<Self>,
        damage_from_parent: AccessibilityDamage,
        hidden: bool,
        context: &AccessibilityContext,
        tree: &mut AccessibilityTree,
        update: &mut AccessibilityUpdate<'update>,
    ) -> LocalAccessibilityDamage {
        let mut local_damage = LocalAccessibilityDamage::empty();

        let dom_node = update.take_dom_node(&self.id);
        let damage = update.take_damage(&self.id) | damage_from_parent;
        let mut children_changed = false;

        if let Some(dom_node) = dom_node {
            local_damage.insert(self.update_properties_and_children_from_dom_node(
                &ref_self, &dom_node, damage, tree, update,
            ));
            local_damage
                .insert(self.update_node_from_layout(&dom_node, damage, hidden, context, update));

            if local_damage.contains(LocalAccessibilityDamage::SubtreeChanged) {
                children_changed = true;
            }

            self.dirty_state -= DirtyState::HasDamage;
        }

        let layout_damage = damage & AccessibilityDamage::Layout;
        if self.dirty_state.descendant_has_damage() || !layout_damage.is_empty() {
            for child_node in self.children() {
                let child_node_ref = child_node.clone();
                let mut child_node = child_node.borrow_mut();
                let child_local_damage = child_node.update_subtree(
                    child_node_ref,
                    layout_damage,
                    hidden || self.is_hidden(),
                    context,
                    tree,
                    update,
                );
                if !child_local_damage.is_empty() {
                    local_damage.insert(LocalAccessibilityDamage::SubtreeChanged);
                    if child_local_damage.contains(LocalAccessibilityDamage::VisibilityChanged) {
                        children_changed = true;
                    }
                }
            }
        }
        self.dirty_state -= DirtyState::DescendantHasDamage;

        if children_changed && let Some(scroll_offset) = self.scroll_offset {
            // If this node may have new, or newly-visible, children, update their scroll offsets.
            self.set_scroll_offset(scroll_offset, update);
        }

        local_damage.insert(self.update_node_local(local_damage, update));

        if self.dirty_state.updated() {
            update.add(self);
        }

        local_damage
    }

    /// Update each of this node's ancestors based on changes which have already been applied in the
    /// tree.
    pub(super) fn update_ancestors(
        &self,
        local_damage: LocalAccessibilityDamage,
        update: &mut AccessibilityUpdate,
    ) {
        if local_damage.is_empty() {
            return;
        }
        for node in self.ancestors() {
            let mut node = node.borrow_mut();
            node.update_node_local(LocalAccessibilityDamage::SubtreeChanged, update);
            node.dirty_state -= DirtyState::DescendantHasDamage;
            if node.dirty_state.updated() {
                update.add(&mut node);
            }
        }
    }

    pub(super) fn initialize_from_dom_node(&mut self, dom_node: &ServoLayoutNode) {
        self.opaque_node = Some(dom_node.opaque());
        if let Some(dom_element) = dom_node.as_element() {
            let local_name = dom_element.local_name().to_ascii_lowercase();
            self.set_html_tag(&local_name);
        }
    }

    /// Update the given [`AccessibilityNode`] from its corresponding DOM node and
    /// [`AccessibilityDamage`].
    /// If it has new children, those will be created here, but not yet populated.
    // Any changed nodes will be added to the given [`AccessibilityUpdate`].
    fn update_properties_and_children_from_dom_node<'update>(
        &mut self,
        ref_self: &ArcRefCell<Self>,
        dom_node: &ServoLayoutNode<'update>,
        dom_damage: AccessibilityDamage,
        tree: &mut AccessibilityTree,
        update: &mut AccessibilityUpdate<'update>,
    ) -> LocalAccessibilityDamage {
        let mut local_damage = LocalAccessibilityDamage::empty();

        if !dom_damage.intersects(
            AccessibilityDamage::Node | AccessibilityDamage::Children | AccessibilityDamage::Layout,
        ) {
            return local_damage;
        }

        // We check for layout damage here because we need to walk the DOM children of nodes with
        // layout damage in order to be able to recompute their bounds. Text nodes have neither
        // bounds nor child nodes, so if the only damage is layout, we can early return here.
        if dom_damage == AccessibilityDamage::Layout && dom_node.is_text_node() {
            return local_damage;
        }

        update.counters.nodes_updated_from_dom += 1;

        if dom_damage.intersects(AccessibilityDamage::Node) {
            local_damage.insert(self.update_properties_from_dom_node(dom_node, tree));
        }

        if dom_damage.intersects(AccessibilityDamage::Children | AccessibilityDamage::Layout) {
            // If this node has damage from layout, this ensures that all of its children have
            // their corresponding DOM nodes in `update`.
            local_damage
                .insert(self.update_children_from_dom_node(ref_self, dom_node, tree, update));
        }

        local_damage
    }

    /// Update this node's [`Self::children`] from its corresponding DOM node.
    /// If it has new children, those will be created here, but not yet populated.
    fn update_children_from_dom_node<'update>(
        &mut self,
        ref_self: &ArcRefCell<AccessibilityNode>,
        dom_node: &ServoLayoutNode<'update>,
        tree: &mut AccessibilityTree,
        update: &mut AccessibilityUpdate<'update>,
    ) -> LocalAccessibilityDamage {
        let modal_dialog_and_ancestors = tree.modal_dialog_and_ancestors().to_vec();
        let mut remaining_dom_children = dom_node
            .flat_tree_children()
            .filter(|dom_child| !is_inert_child(&modal_dialog_and_ancestors, dom_node, dom_child))
            .peekable();
        let mut old_child_ids = self.child_ids().iter().peekable();

        // Iterate over existing children and DOM children while they match. No action is necessary
        // for these nodes.
        let mut unchanged_count = 0usize;
        while let Some(&old_id) = old_child_ids.peek() &&
            let Some(dom_child) = remaining_dom_children.peek()
        {
            if tree.existing_id_for_opaque(dom_child.opaque()) == Some(*old_id) {
                update.insert_dom_node(*old_id, *dom_child);
                unchanged_count += 1;
                old_child_ids.next();
                remaining_dom_children.next();
            } else {
                break;
            }
        }

        // If we iterated over all the DOM children without finding any changes, we're done.
        if old_child_ids.peek().is_none() && remaining_dom_children.peek().is_none() {
            return LocalAccessibilityDamage::empty();
        }

        // Remove all child nodes after the first `unchanged_count`.
        self.child_nodes.truncate(unchanged_count);
        let mut new_child_ids = Vec::from(self.child_ids());
        for removed_child_id in new_child_ids.split_off(unchanged_count) {
            update.set_tree_state_change(removed_child_id, TreeChange::Removed);
        }

        // Then, (re-)add all the remaining DOM children. Note that this means that some children
        // may end up being "Moved" even though they haven't changed parents, and may even be in the
        // same position as previously.
        let weak_self = ref_self.downgrade();
        for dom_child in remaining_dom_children {
            let (child_id, child_ref) = tree.get_or_create_node(&dom_child, update);
            // TODO(#47162): Since we need to update bounds for all nodes, we need to ensure every
            // AccessibilityNode has a corresponding DOM node available to be retrieved from the
            // AccessibilityUpdate. Once we no longer update bounds on all nodes, we won't need to
            // add all nodes like this.
            update.insert_dom_node(child_id, dom_child);

            // Update self.child_nodes in place.
            self.child_nodes.push(child_ref.clone());
            new_child_ids.push(child_id);

            let mut child = child_ref.borrow_mut();
            child.parent_node = Some(weak_self.clone());

            if update.is_new(&child_id) {
                self.dirty_state |= DirtyState::DescendantHasDamage;
            } else {
                update.set_tree_state_change(child_id, TreeChange::PendingMove);
            }

            self.dirty_state
                .propagate_descendant_has_damage(child.dirty_state);
        }

        // We can't update the AccessKit node's `children` in place, so we build up the full list
        // and then set it here.
        self.accesskit_node.set_children(new_child_ids);
        self.dirty_state |= DirtyState::Updated;

        LocalAccessibilityDamage::SubtreeChanged
    }

    /// Update this node's properties from its corresponding DOM node.
    fn update_properties_from_dom_node(
        &mut self,
        dom_node: &ServoLayoutNode,
        tree: &mut AccessibilityTree,
    ) -> LocalAccessibilityDamage {
        let mut local_damage = LocalAccessibilityDamage::empty();
        local_damage.insert(self.set_role(role_from_dom_node(dom_node)));
        if dom_node.type_id() == Some(LayoutNodeType::Text) {
            let text_content = dom_node.text_content();
            trace!("node text content = {text_content:?}");
            // FIXME: this should take into account editing selection units (grapheme clusters?)
            local_damage.insert(self.set_value(&text_content));
        }

        if let Some(element) = dom_node.as_element() {
            // Record this element's `id` so aria-labelledby idrefs can resolve to it.
            let element_id: Option<Box<str>> = element
                .attribute_as_str(&ns!(), &local_name!("id"))
                .filter(|value| !value.is_empty())
                .map(Box::from);
            if element_id != self.element_id {
                tree.set_element_id_for_node(
                    self.id,
                    self.element_id.as_deref(),
                    element_id.as_deref(),
                );
                self.element_id = element_id;
            }

            // Explicit accessible name (aria-label/alt/title). When present it wins over
            // name-from-contents.
            let explicit_name = explicit_name(&element).map(Box::from);
            if explicit_name != self.explicit_name {
                self.explicit_name = explicit_name;
                local_damage.insert(LocalAccessibilityDamage::NameSourceChanged);
            }

            // aria-labelledby is resolved later (it needs the tree's id map); just record it.
            let labelledby: Option<Box<str>> = element
                .attribute_as_str(&ns!(), &local_name!("aria-labelledby"))
                .filter(|value| !value.trim().is_empty())
                .map(Box::from);
            if labelledby != self.labelledby {
                tree.set_node_has_labelledby(self.id, labelledby.is_some());
                if labelledby.is_none() && self.labelledby_name.take().is_some() {
                    local_damage.insert(LocalAccessibilityDamage::NameSourceChanged);
                }
                self.labelledby = labelledby;
            }

            self.apply_states(&element);
        }

        local_damage
    }

    /// Populate ARIA / native boolean and toggle states from the element's attributes, and
    /// advertise the actions an assistive technology can invoke on this node.
    fn apply_states(&mut self, element: &ServoLayoutElement<'_>) {
        let has = |name: &LocalName| element.attribute_as_str(&ns!(), name);
        let mut changed = false;
        let mut set_flag = |node: &mut accesskit::Node,
                            value: bool,
                            get: fn(&accesskit::Node) -> bool,
                            set: fn(&mut accesskit::Node),
                            clear: fn(&mut accesskit::Node)| {
            if get(node) == value {
                return;
            }
            if value {
                set(node)
            } else {
                clear(node)
            }
            changed = true;
        };

        let disabled = has(&local_name!("disabled")).is_some() ||
            has(&local_name!("aria-disabled")) == Some("true");
        set_flag(
            &mut self.accesskit_node,
            disabled,
            accesskit::Node::is_disabled,
            accesskit::Node::set_disabled,
            accesskit::Node::clear_disabled,
        );
        let required = has(&local_name!("required")).is_some() ||
            has(&local_name!("aria-required")) == Some("true");
        set_flag(
            &mut self.accesskit_node,
            required,
            accesskit::Node::is_required,
            accesskit::Node::set_required,
            accesskit::Node::clear_required,
        );
        let read_only = has(&local_name!("readonly")).is_some() ||
            has(&local_name!("aria-readonly")) == Some("true");
        set_flag(
            &mut self.accesskit_node,
            read_only,
            accesskit::Node::is_read_only,
            accesskit::Node::set_read_only,
            accesskit::Node::clear_read_only,
        );

        let tristate = |value: Option<&str>| match value {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        let expanded = tristate(has(&local_name!("aria-expanded")));
        if self.accesskit_node.is_expanded() != expanded {
            match expanded {
                Some(expanded) => self.accesskit_node.set_expanded(expanded),
                None => self.accesskit_node.clear_expanded(),
            }
            changed = true;
        }
        let selected = tristate(has(&local_name!("aria-selected")));
        if self.accesskit_node.is_selected() != selected {
            match selected {
                Some(selected) => self.accesskit_node.set_selected(selected),
                None => self.accesskit_node.clear_selected(),
            }
            changed = true;
        }

        // Checkbox/radio `checked`, or tri-state `aria-checked`.
        let toggled = match has(&local_name!("aria-checked")) {
            Some("true") => Some(accesskit::Toggled::True),
            Some("false") => Some(accesskit::Toggled::False),
            Some("mixed") => Some(accesskit::Toggled::Mixed),
            _ if has(&local_name!("checked")).is_some() => Some(accesskit::Toggled::True),
            _ => None,
        };
        if self.accesskit_node.toggled() != toggled {
            match toggled {
                Some(toggled) => self.accesskit_node.set_toggled(toggled),
                None => self.accesskit_node.clear_toggled(),
            }
            changed = true;
        }

        // Advertise the actions an assistive technology can invoke on this node, so screen
        // readers offer activation/focus. The embedder forwards these back to the DOM.
        let role = self.accesskit_node.role();
        let clickable = matches!(
            role,
            Role::Button |
                Role::DefaultButton |
                Role::Link |
                Role::CheckBox |
                Role::RadioButton |
                Role::Switch |
                Role::MenuItem |
                Role::MenuItemCheckBox |
                Role::MenuItemRadio |
                Role::Tab |
                Role::ListBoxOption |
                Role::TreeItem |
                Role::ColorWell |
                Role::ComboBox
        );
        let focusable = clickable ||
            matches!(
                role,
                Role::TextInput |
                    Role::MultilineTextInput |
                    Role::SearchInput |
                    Role::NumberInput |
                    Role::EmailInput |
                    Role::PasswordInput |
                    Role::PhoneNumberInput |
                    Role::UrlInput |
                    Role::DateInput |
                    Role::DateTimeInput |
                    Role::TimeInput |
                    Role::MonthInput |
                    Role::WeekInput |
                    Role::Slider |
                    Role::SpinButton
            );
        for (action, supported) in [
            (Action::Click, clickable),
            (Action::Focus, focusable),
            (Action::Blur, focusable),
            (Action::ScrollIntoView, clickable || focusable),
        ] {
            if self.accesskit_node.supports_action(action) == supported {
                continue;
            }
            if supported {
                self.accesskit_node.add_action(action);
            } else {
                self.accesskit_node.remove_action(action);
            }
            changed = true;
        }

        if changed {
            self.dirty_state |= DirtyState::Updated;
        }
    }

    /// Update this node's bounds from the current layout geometry.
    fn update_node_from_layout(
        &mut self,
        dom_node: &ServoLayoutNode<'_>,
        layout_damage: AccessibilityDamage,
        hidden: bool,
        context: &AccessibilityContext,
        update: &mut AccessibilityUpdate,
    ) -> LocalAccessibilityDamage {
        let mut local_damage = LocalAccessibilityDamage::empty();

        // Don't update bounds on nodes in hidden subtrees.
        if hidden || !layout_damage.intersects(AccessibilityDamage::Layout) {
            return local_damage;
        }

        if let Some(dom_element) = dom_node.as_element() &&
            dom_element.style_data().is_some()
        {
            let data = dom_element.element_data();
            let style = data.styles.primary();
            if style.get_display().is_none() {
                self.clear_bounds();
                local_damage.insert(self.set_hidden());
            } else {
                local_damage.insert(self.clear_hidden());
            }
        }

        if self.is_hidden() {
            return local_damage;
        }

        update.counters.nodes_updated_bounds += 1;

        // Border box without transforms. Bounds are in CSS pixels, relative to the document origin;
        // scroll containers set translations on their child nodes, and the embedder's graft node
        // carries the transform that composes them into AccessKit's coordinate space (see the
        // "Coordinates" section of
        // <https://docs.rs/accesskit/latest/accesskit/struct.Node.html>).
        // TODO(#47166): This doesn't take any CSS transforms into account.
        let bounds = process_box_area_request(
            context.layout_thread,
            *dom_node,
            BoxAreaType::Border,
            BoxAreaInclusion::Inlines,
        )
        .map(au_rect_to_accesskit_rect);

        // For now only nodes with a box of their own get bounds; anything else, including
        // `display: none` content, gets its bounds cleared. That leaves two kinds of nodes
        // without geometry which assistive technology would like to have some:
        //
        // TODO(#47164): A text node never has bounds of its own: `LayoutBox::Text` has no
        // `LayoutBoxBase`, and `Fragment::Text` has no box area, so the query above always returns
        // `None` for one. Text nodes should get the union of the rectangles of their own
        // `Fragment::Text` fragments, once `cumulative_box_area_rect()` can handle those.
        //
        // TODO(#47163): A `display: contents` element generates no box either. Other
        // engines (Blink, WebKit, Gecko) compute its bounds as the union of the bounding boxes of
        // its rendered descendants.
        match bounds {
            Some(bounds) => self.set_bounds(bounds),
            None => self.clear_bounds(),
        }
        local_damage
    }

    /// Update this node's properties based on changes already made to the accessibility tree.
    /// For example, if there were nodes added or removed in its subtree, its computed text may have
    /// changed, so that will be recomputed here.
    /// If any changes are made, add this node to the given [`AccessibilityUpdate`].
    fn update_node_local(
        &mut self,
        local_damage: LocalAccessibilityDamage,
        update: &mut AccessibilityUpdate,
    ) -> LocalAccessibilityDamage {
        let mut new_damage = LocalAccessibilityDamage::empty();
        if local_damage.is_empty() {
            return new_damage;
        }
        update.counters.nodes_updated_from_tree += 1;

        if local_damage.intersects(
            LocalAccessibilityDamage::SubtreeChanged |
                LocalAccessibilityDamage::RoleChanged |
                LocalAccessibilityDamage::NameSourceChanged,
        ) {
            new_damage.insert(self.recompute_name());
        }

        new_damage
    }

    /// Recompute this node's accessible name from its sources, in precedence order:
    /// `aria-labelledby`, then the explicit `aria-label`/`alt`/`title`, then name-from-contents.
    fn recompute_name(&mut self) -> LocalAccessibilityDamage {
        if let Some(name) = self
            .labelledby_name
            .clone()
            .or_else(|| self.explicit_name.clone())
        {
            return self.set_label(&name);
        }
        if let Some(text) = self.label_from_descendants() {
            self.set_label(text.as_str())
        } else {
            self.clear_label()
        }
    }

    /// The raw `aria-labelledby` idref list of this node, if any.
    pub(super) fn labelledby(&self) -> Option<&str> {
        self.labelledby.as_deref()
    }

    /// The element `id` this node is registered under in the tree's id map, if any.
    pub(super) fn element_id(&self) -> Option<&str> {
        self.element_id.as_deref()
    }

    /// Set the accessible name resolved from `aria-labelledby` (or `None` if it resolved to
    /// nothing), recomputing the node's name if that changed.
    pub(super) fn set_labelledby_name(&mut self, name: Option<Box<str>>) {
        if self.labelledby_name == name {
            return;
        }
        self.labelledby_name = name;
        self.recompute_name();
    }

    /// The text used when this node is referenced by `aria-labelledby`: its own accessible name
    /// if it has one, else the concatenation of its descendant text (any role, unlike
    /// name-from-contents, which is role-gated).
    pub(super) fn accessible_text(&self) -> Option<String> {
        if let Some(label) = self.label() {
            let label = label.trim();
            if !label.is_empty() {
                return Some(label.to_owned());
            }
        }
        if let Some(value) = self.value() &&
            self.role() == Role::TextRun
        {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
        let text = self.text_from_descendants();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn text_from_descendants(&self) -> String {
        let mut children = VecDeque::from_iter(self.children().cloned());
        let mut text = String::new();
        while let Some(child) = children.pop_front() {
            let child = child.borrow();
            if child.is_hidden() {
                continue;
            }
            match child.role() {
                Role::TextRun => {
                    if let Some(child_text) = child.value() {
                        text.push_str(child_text);
                    }
                },
                _ => {
                    for node in child.children().rev() {
                        children.push_front(node.clone());
                    }
                },
            }
        }
        text
    }

    fn label_from_descendants(&self) -> Option<String> {
        if !NAME_FROM_CONTENTS_ROLES.contains(&self.role()) {
            return None;
        }
        Some(self.text_from_descendants().trim().to_owned())
    }

    pub(super) fn print(&self, print_tree: &mut PrintTree, tree: &AccessibilityTree) {
        let focused = if tree.focused_node_id() == Some(self.id) {
            "[focused] "
        } else {
            ""
        };
        let node_string = format!("{focused}{self:?}");

        if self.child_nodes.is_empty() {
            print_tree.add_item(node_string);
            return;
        }

        print_tree.new_level(node_string);

        for child in self.children() {
            child.borrow().print(print_tree, tree);
        }
        print_tree.end_level();
    }

    pub(super) fn clone_accesskit_node(&self) -> accesskit::Node {
        self.accesskit_node.clone()
    }

    pub(super) fn id(&self) -> NodeId {
        self.id
    }

    pub(super) fn opaque_node(&self) -> Option<OpaqueNode> {
        self.opaque_node
    }

    pub(super) fn dirty_state(&self) -> DirtyState {
        self.dirty_state
    }

    pub(super) fn add_dirty_state(&mut self, state: DirtyState) {
        self.dirty_state |= state;
    }

    pub(super) fn remove_dirty_state(&mut self, state: DirtyState) {
        self.dirty_state -= state;
    }

    pub(super) fn parent(&self) -> Option<ArcRefCell<AccessibilityNode>> {
        self.parent_node.as_ref().and_then(|weak| weak.upgrade())
    }

    pub(super) fn children(
        &self,
    ) -> impl DoubleEndedIterator<Item = &ArcRefCell<AccessibilityNode>> {
        self.child_nodes.iter()
    }

    #[cfg(test)]
    pub(super) fn set_children_for_testing(
        &mut self,
        child_ids: Vec<NodeId>,
        child_nodes: Vec<ArcRefCell<AccessibilityNode>>,
    ) {
        self.accesskit_node.set_children(child_ids);
        self.child_nodes = child_nodes;
    }

    pub(super) fn ancestors(&self) -> impl Iterator<Item = ArcRefCell<AccessibilityNode>> {
        AccessibilityNodeIterator::new(self.parent(), |node| node.parent_node.clone()?.upgrade())
    }

    pub(super) fn child_ids(&self) -> &[NodeId] {
        self.accesskit_node.children()
    }

    pub(super) fn set_scroll_offset(
        &mut self,
        offset: LayoutVector2D,
        update: &mut AccessibilityUpdate,
    ) {
        self.scroll_offset = Some(offset);
        let transform = scroll_offset_to_affine(offset);
        for child in self.children() {
            let mut child = child.borrow_mut();
            if child.is_hidden() {
                continue;
            }
            child.set_transform(transform);
            if child.dirty_state.updated() {
                update.add(&mut child);
            }
        }
    }

    pub(super) fn role(&self) -> Role {
        self.accesskit_node.role()
    }

    pub(super) fn set_role(&mut self, role: Role) -> LocalAccessibilityDamage {
        if role == self.accesskit_node.role() {
            return LocalAccessibilityDamage::empty();
        }
        self.accesskit_node.set_role(role);
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::RoleChanged
    }

    fn label(&self) -> Option<&str> {
        self.accesskit_node.label()
    }

    fn set_label(&mut self, label: &str) -> LocalAccessibilityDamage {
        if Some(label) == self.accesskit_node.label() {
            return LocalAccessibilityDamage::empty();
        }
        self.accesskit_node.set_label(label);
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::TextChanged
    }

    fn clear_label(&mut self) -> LocalAccessibilityDamage {
        if self.accesskit_node.label().is_none() {
            return LocalAccessibilityDamage::empty();
        }
        self.accesskit_node.clear_label();
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::TextChanged
    }

    fn html_tag(&self) -> Option<&str> {
        self.accesskit_node.html_tag()
    }

    fn set_html_tag(&mut self, html_tag: &str) {
        if Some(html_tag) == self.accesskit_node.html_tag() {
            return;
        }
        self.accesskit_node.set_html_tag(html_tag);
        self.dirty_state |= DirtyState::Updated;
    }

    fn value(&self) -> Option<&str> {
        self.accesskit_node.value()
    }

    fn set_value(&mut self, value: &str) -> LocalAccessibilityDamage {
        if Some(value) == self.accesskit_node.value() {
            return LocalAccessibilityDamage::empty();
        }
        self.accesskit_node.set_value(value);
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::TextChanged
    }

    fn is_hidden(&self) -> bool {
        self.accesskit_node.is_hidden()
    }

    fn set_hidden(&mut self) -> LocalAccessibilityDamage {
        if self.is_hidden() {
            return LocalAccessibilityDamage::empty();
        }
        self.accesskit_node.set_hidden();
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::VisibilityChanged
    }

    fn clear_hidden(&mut self) -> LocalAccessibilityDamage {
        if !self.is_hidden() {
            return LocalAccessibilityDamage::empty();
        }

        self.accesskit_node.clear_hidden();
        self.dirty_state |= DirtyState::Updated;
        LocalAccessibilityDamage::VisibilityChanged
    }

    fn bounds(&self) -> Option<accesskit::Rect> {
        self.accesskit_node.bounds()
    }

    fn set_bounds(&mut self, bounds: accesskit::Rect) {
        if Some(bounds) == self.accesskit_node.bounds() {
            return;
        }
        self.accesskit_node.set_bounds(bounds);
        self.dirty_state |= DirtyState::Updated;
    }

    fn clear_bounds(&mut self) {
        if self.accesskit_node.bounds().is_none() {
            return;
        }
        self.accesskit_node.clear_bounds();
        self.dirty_state |= DirtyState::Updated;
    }

    fn set_transform(&mut self, transform: Affine) {
        // TODO(#47166): Right now a node will only ever have a single transform from a scroll
        // container, if any. Once we correctly support CSS transforms, a node may have multiple
        // transforms, which we'll need to be able to combine.
        if self.accesskit_node.transform() == Some(&transform) {
            return;
        }
        if transform == Affine::IDENTITY {
            self.clear_transform();
            return;
        }
        self.accesskit_node.set_transform(transform);
        self.dirty_state |= DirtyState::Updated;
    }

    fn clear_transform(&mut self) {
        if self.accesskit_node.transform().is_none() {
            return;
        }
        self.accesskit_node.clear_transform();
        self.dirty_state |= DirtyState::Updated;
    }

    pub(super) fn assert_integrity(&self, expected_parent: Option<WeakRefCell<AccessibilityNode>>) {
        debug_assert!(pref!(expensive_accessibility_test_assertions_enabled));

        if let Some(actual_parent) = &self.parent_node {
            let expected = expected_parent.expect("Actual parent but no expected parent");
            let expected = expected.upgrade().expect("Expected parent was dropped");
            let actual = actual_parent.upgrade().expect("Actual parent was dropped");
            assert!(actual.ptr_eq(&expected));
        } else {
            assert!(
                expected_parent.is_none(),
                "Expected parent but no actual parent"
            );
        }

        assert!(
            self.dirty_state.is_empty(),
            "{self:?} has dirty state {:?}",
            self.dirty_state
        );

        let children_ids: Vec<_> = self.children().map(|child| child.borrow().id).collect();
        assert_eq!(
            children_ids,
            self.child_ids(),
            "children() IDs didn't match child_ids() for {self:?}"
        );
    }
}

impl Debug for AccessibilityNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_hidden() {
            write!(f, "[hidden] ")?;
        }
        write!(f, "{:?}: {:?}", self.id, self.role())?;
        if let Some(html_tag) = self.html_tag() {
            write!(f, " (html_tag: {html_tag:?})")?;
        }
        if let Some(label) = self.label() {
            write!(f, "\nlabel: {label:?}")?;
        }
        if let Some(bounds) = self.bounds() {
            write!(f, "\nbounds: {bounds:?}")?;
        }
        if !self.child_ids().is_empty() {
            write!(f, "\nchildren: {:?}", self.child_ids())?;
        }
        Ok(())
    }
}

/// Whether `dom_child` of `dom_node` is inert, which hides it and its subtree from assistive
/// technology. `modal_dialog_and_ancestors` is the modal dialog that blocks the document followed
/// by its flat tree ancestors, if any. <https://html.spec.whatwg.org/multipage/#inert-subtrees>
fn is_inert_child(
    modal_dialog_and_ancestors: &[OpaqueNode],
    dom_node: &ServoLayoutNode<'_>,
    dom_child: &ServoLayoutNode<'_>,
) -> bool {
    let leads_to_modal_dialog = modal_dialog_and_ancestors.contains(&dom_child.opaque());
    // Ancestors of the modal dialog are inert themselves, but stay in the tree to hold it.
    if modal_dialog_and_ancestors
        .get(1..)
        .is_some_and(|ancestors| ancestors.contains(&dom_node.opaque())) &&
        !leads_to_modal_dialog
    {
        return true;
    }
    // The modal dialog escapes the inertness of inert ancestors.
    !leads_to_modal_dialog &&
        dom_child.as_html_element().is_some_and(|element| {
            element
                .attribute_as_str(&ns!(), &local_name!("inert"))
                .is_some()
        })
}

/// <https://w3c.github.io/aria/#host_general_role>
///
/// The first recognised token wins. Tokens in the shared safe-list ([`SUPPORTED_ARIA_ROLES`]) map
/// through [`ARIA_ROLE_TO_ACCESSKIT_ROLE`]; widget and other roles outside it fall back to
/// [`aria_role_to_accesskit`].
fn role_from_role_attribute(dom_element: &ServoLayoutElement<'_>) -> Option<Role> {
    let role_attribute = dom_element.attribute(&ns!(), &local_name!("role"))?;
    role_attribute
        .as_tokens()
        .iter()
        .find_map(|role_name_in_attribute| {
            if SUPPORTED_ARIA_ROLES.contains(role_name_in_attribute) {
                return ARIA_ROLE_TO_ACCESSKIT_ROLE
                    .get(role_name_in_attribute)
                    .cloned();
            }
            aria_role_to_accesskit(role_name_in_attribute)
        })
}

fn role_from_dom_node(dom_node: &ServoLayoutNode<'_>) -> Role {
    let Some(dom_element) = dom_node.as_element() else {
        return if dom_node.type_id() == Some(LayoutNodeType::Text) {
            Role::TextRun
        } else {
            Role::GenericContainer
        };
    };

    // An explicit ARIA `role` attribute overrides the native role.
    if let Some(role) = role_from_role_attribute(&dom_element) {
        return role;
    }

    let local_name = dom_element.local_name().to_ascii_lowercase();

    // Elements whose native role depends on their attributes.
    if local_name == local_name!("input") {
        return input_role(&dom_element);
    }
    if local_name == local_name!("a") || local_name == local_name!("area") {
        // Only a placed anchor (with `href`) is a link; a bare `<a>` is a generic container.
        return if dom_element
            .attribute(&ns!(), &local_name!("href"))
            .is_some()
        {
            Role::Link
        } else {
            Role::GenericContainer
        };
    }
    if local_name == local_name!("img") {
        // `alt=""` marks the image decorative (presentational).
        return match dom_element.attribute_as_str(&ns!(), &local_name!("alt")) {
            Some("") => Role::GenericContainer,
            _ => Role::Image,
        };
    }

    // Static HTML-element → role map for everything else.
    *HTML_ELEMENT_ROLE_MAPPINGS
        .get(&local_name)
        .unwrap_or(&Role::GenericContainer)
}

/// Map an `<input>`'s `type` to its accessibility role (per HTML-AAM).
fn input_role(element: &ServoLayoutElement<'_>) -> Role {
    let input_type = element
        .attribute_as_str(&ns!(), &local_name!("type"))
        .unwrap_or("text")
        .to_ascii_lowercase();
    match input_type.as_str() {
        "checkbox" => Role::CheckBox,
        "radio" => Role::RadioButton,
        "button" | "submit" | "reset" | "image" => Role::Button,
        "range" => Role::Slider,
        "number" => Role::NumberInput,
        "email" => Role::EmailInput,
        "tel" => Role::PhoneNumberInput,
        "url" => Role::UrlInput,
        "password" => Role::PasswordInput,
        "search" => Role::SearchInput,
        "date" => Role::DateInput,
        "datetime-local" => Role::DateTimeInput,
        "time" => Role::TimeInput,
        "month" => Role::MonthInput,
        "week" => Role::WeekInput,
        "color" => Role::ColorWell,
        "hidden" => Role::GenericContainer,
        // `text` and any unknown/future type default to a plain text field.
        _ => Role::TextInput,
    }
}

/// Map an ARIA `role` token outside [`SUPPORTED_ARIA_ROLES`] to an accesskit [`Role`], or `None`
/// if unrecognised.
fn aria_role_to_accesskit(role: &str) -> Option<Role> {
    Some(match role {
        "button" => Role::Button,
        "link" => Role::Link,
        "checkbox" => Role::CheckBox,
        "radio" => Role::RadioButton,
        "switch" => Role::Switch,
        "textbox" => Role::TextInput,
        "searchbox" => Role::SearchInput,
        "combobox" => Role::ComboBox,
        "listbox" => Role::ListBox,
        "option" => Role::ListBoxOption,
        "list" => Role::List,
        "listitem" => Role::ListItem,
        "menu" => Role::Menu,
        "menubar" => Role::MenuBar,
        "menuitem" => Role::MenuItem,
        "menuitemcheckbox" => Role::MenuItemCheckBox,
        "menuitemradio" => Role::MenuItemRadio,
        "tab" => Role::Tab,
        "tablist" => Role::TabList,
        "tabpanel" => Role::TabPanel,
        "dialog" | "alertdialog" => Role::Dialog,
        "alert" => Role::Alert,
        "status" => Role::Status,
        "progressbar" => Role::ProgressIndicator,
        "meter" => Role::Meter,
        "slider" => Role::Slider,
        "spinbutton" => Role::SpinButton,
        "scrollbar" => Role::ScrollBar,
        "separator" => Role::Splitter,
        "heading" => Role::Heading,
        "img" | "image" => Role::Image,
        "figure" => Role::Figure,
        "banner" => Role::Banner,
        "navigation" => Role::Navigation,
        "main" => Role::Main,
        "complementary" => Role::Complementary,
        "contentinfo" => Role::ContentInfo,
        "region" => Role::Region,
        "search" => Role::Search,
        "form" => Role::Form,
        "article" => Role::Article,
        "document" => Role::Document,
        "application" => Role::Application,
        "table" => Role::Table,
        "grid" => Role::Grid,
        "row" => Role::Row,
        "rowgroup" => Role::RowGroup,
        "cell" => Role::Cell,
        "gridcell" => Role::GridCell,
        "columnheader" => Role::ColumnHeader,
        "rowheader" => Role::RowHeader,
        "tree" => Role::Tree,
        "treegrid" => Role::TreeGrid,
        "treeitem" => Role::TreeItem,
        "group" => Role::Group,
        "radiogroup" => Role::RadioGroup,
        "toolbar" => Role::Toolbar,
        "tooltip" => Role::Tooltip,
        "note" => Role::Note,
        "log" => Role::Log,
        "marquee" => Role::Marquee,
        "timer" => Role::Timer,
        "term" => Role::Term,
        "definition" => Role::Definition,
        "feed" => Role::Feed,
        "presentation" | "none" => Role::GenericContainer,
        _ => return None,
    })
}

/// The explicit accessible name from attributes, in HTML-AAM precedence: `aria-label`, then
/// (for images) `alt`, then `title`. `aria-labelledby` is resolved separately by the tree, since
/// it needs cross-tree id resolution.
fn explicit_name(element: &ServoLayoutElement<'_>) -> Option<String> {
    let non_empty = |s: &str| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    element
        .attribute_as_str(&ns!(), &local_name!("aria-label"))
        .and_then(non_empty)
        .or_else(|| {
            element
                .attribute_as_str(&ns!(), &local_name!("alt"))
                .and_then(non_empty)
        })
        .or_else(|| {
            element
                .attribute_as_str(&ns!(), &local_name!("title"))
                .and_then(non_empty)
        })
}

struct AccessibilityNodeIterator<I>
where
    I: Fn(&AccessibilityNode) -> Option<ArcRefCell<AccessibilityNode>>,
{
    next_value: Option<ArcRefCell<AccessibilityNode>>,
    next_fn: I,
}

impl<I> AccessibilityNodeIterator<I>
where
    I: Fn(&AccessibilityNode) -> Option<ArcRefCell<AccessibilityNode>>,
{
    fn new(next_value: Option<ArcRefCell<AccessibilityNode>>, next_fn: I) -> Self {
        AccessibilityNodeIterator {
            next_value,
            next_fn,
        }
    }
}

impl<I> Iterator for AccessibilityNodeIterator<I>
where
    I: Fn(&AccessibilityNode) -> Option<ArcRefCell<AccessibilityNode>>,
{
    type Item = ArcRefCell<AccessibilityNode>;

    fn next(&mut self) -> Option<Self::Item> {
        let next_value = self.next_value.take();
        self.next_value = next_value
            .as_ref()
            .and_then(|node| (self.next_fn)(&node.borrow()));
        next_value
    }
}

impl DirtyState {
    pub(super) fn updated(&self) -> bool {
        self.contains(DirtyState::Updated)
    }

    pub(super) fn descendant_has_damage(&self) -> bool {
        self.contains(DirtyState::DescendantHasDamage)
    }

    pub(super) fn propagate_descendant_has_damage(&mut self, child_dirty_state: DirtyState) {
        if child_dirty_state.self_or_descendant_has_damage() {
            self.insert(DirtyState::DescendantHasDamage)
        }
    }

    pub(super) fn self_or_descendant_has_damage(&self) -> bool {
        self.intersects(DirtyState::HasDamage | DirtyState::DescendantHasDamage)
    }
}

static HTML_ELEMENT_ROLE_MAPPINGS: LazyLock<FxHashMap<LocalName, Role>> = LazyLock::new(|| {
    [
        // `a`, `area`, `img` and `input` are attribute-dependent, see `role_from_dom_node`.
        (local_name!("article"), Role::Article),
        (local_name!("aside"), Role::Complementary),
        (local_name!("body"), Role::RootWebArea),
        (local_name!("footer"), Role::ContentInfo),
        (local_name!("h1"), Role::Heading),
        (local_name!("h2"), Role::Heading),
        (local_name!("h3"), Role::Heading),
        (local_name!("h4"), Role::Heading),
        (local_name!("h5"), Role::Heading),
        (local_name!("h6"), Role::Heading),
        (local_name!("header"), Role::Banner),
        (local_name!("hr"), Role::Splitter),
        (local_name!("main"), Role::Main),
        (local_name!("nav"), Role::Navigation),
        (local_name!("p"), Role::Paragraph),
        // Form controls + labels.
        (local_name!("button"), Role::Button),
        (local_name!("select"), Role::ComboBox),
        (local_name!("textarea"), Role::MultilineTextInput),
        (local_name!("label"), Role::Label),
        (local_name!("form"), Role::Form),
        (local_name!("fieldset"), Role::Group),
        (local_name!("legend"), Role::Legend),
        (local_name!("output"), Role::Status),
        (local_name!("progress"), Role::ProgressIndicator),
        (local_name!("meter"), Role::Meter),
        // Lists.
        (local_name!("ul"), Role::List),
        (local_name!("ol"), Role::List),
        (local_name!("menu"), Role::List),
        (local_name!("li"), Role::ListItem),
        (local_name!("dl"), Role::DescriptionList),
        (local_name!("dt"), Role::Term),
        (local_name!("dd"), Role::Definition),
        // Tables.
        (local_name!("table"), Role::Table),
        (local_name!("thead"), Role::RowGroup),
        (local_name!("tbody"), Role::RowGroup),
        (local_name!("tfoot"), Role::RowGroup),
        (local_name!("tr"), Role::Row),
        (local_name!("td"), Role::Cell),
        (local_name!("th"), Role::ColumnHeader),
        (local_name!("caption"), Role::Caption),
        // Sectioning + grouping.
        (local_name!("section"), Role::Section),
        (local_name!("figure"), Role::Figure),
        (local_name!("figcaption"), Role::Caption),
        (local_name!("blockquote"), Role::Blockquote),
        (local_name!("details"), Role::Details),
        (local_name!("summary"), Role::Button),
        (local_name!("dialog"), Role::Dialog),
        // Inline semantics + embedded content.
        (local_name!("code"), Role::Code),
        (local_name!("strong"), Role::Strong),
        (local_name!("em"), Role::Emphasis),
        (local_name!("abbr"), Role::Abbr),
        (local_name!("mark"), Role::Mark),
        (local_name!("time"), Role::Time),
        (local_name!("video"), Role::Video),
        (local_name!("audio"), Role::Audio),
        (local_name!("canvas"), Role::Canvas),
        (local_name!("iframe"), Role::Iframe),
    ]
    .into_iter()
    .collect()
});

/// Roles whose accessible name is computed from their contents when no explicit name is given.
/// <https://w3c.github.io/aria/#namefromcontent>
static NAME_FROM_CONTENTS_ROLES: LazyLock<FxHashSet<Role>> = LazyLock::new(|| {
    [
        Role::Heading,
        Role::Button,
        Role::DefaultButton,
        Role::Link,
        Role::Cell,
        Role::GridCell,
        Role::ColumnHeader,
        Role::RowHeader,
        Role::Row,
        Role::ListItem,
        Role::ListBoxOption,
        Role::MenuItem,
        Role::MenuItemCheckBox,
        Role::MenuItemRadio,
        Role::CheckBox,
        Role::RadioButton,
        Role::Switch,
        Role::Tab,
        Role::TreeItem,
        Role::Tooltip,
        Role::Term,
    ]
    .into_iter()
    .collect()
});
