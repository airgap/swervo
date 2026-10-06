/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `:has()` invalidation. Matching a `:has()` selector flags its anchor and every element the
//! search visits. When an element's state, attributes or place in the tree change, stylo's
//! relative selector invalidator looks the change up in the stylist's map of the features that
//! appear inside `:has()` arguments; only a change that some argument can see walks from there
//! to the flagged anchors, and then only the elements whose selectors depend on the anchor are
//! restyled. This drives the invalidator the way Gecko does, from the
//! `Servo_StyleSet_MaybeInvalidateRelativeSelector*` functions of geckolib's glue, which its
//! RestyleManager calls as the DOM changes.

#![expect(unsafe_code)]

use std::cell::RefCell;
use std::marker::PhantomData;

use html5ever::{LocalName, Namespace, local_name, ns};
use js::context::NoGC;
use layout_api::with_layout_state;
use selectors::Element as _;
use selectors::matching::ElementSelectorFlags;
use style::context::QuirksMode;
use style::dom::TElement;
use style::invalidation::element::element_wrapper::ElementSnapshot;
use style::invalidation::element::invalidation_map::TSStateForInvalidation;
use style::invalidation::element::invalidator::{InvalidationResult, SiblingTraversalMap};
use style::invalidation::element::relative_selector::{
    DomMutationOperation, RelativeSelectorInvalidator,
};
use style::stylist::Stylist;
use stylo_atoms::Atom;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom, LayoutFromRaw};
use crate::dom::element::Element;
use crate::dom::node::{ChildrenMutation, Node, NodeTraits};
use crate::layout_dom::ServoDangerousStyleElement;

const SEARCH_DIRECTION: ElementSelectorFlags =
    ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING;

thread_local! {
    /// The anchors restyled by the invalidation in progress, with whether their later siblings
    /// were restyled too. The invalidator reports them through a plain function pointer, and
    /// the DOM's dirty-root bookkeeping has to wait until it returns.
    static INVALIDATED_ANCHORS: RefCell<Vec<(*const Element, bool)>> =
        const { RefCell::new(Vec::new()) };
}

fn note_invalidated_anchor(anchor: ServoDangerousStyleElement<'_>, result: &InvalidationResult) {
    if !result.has_invalidated_self() &&
        !result.has_invalidated_descendants() &&
        !result.has_invalidated_siblings()
    {
        return;
    }
    let anchor: *const Element = unsafe { anchor.element.as_ref() };
    INVALIDATED_ANCHORS.with_borrow_mut(|anchors| {
        anchors.push((anchor, result.has_invalidated_siblings()))
    });
}

/// Servo's element snapshots stay in the document's pending restyles until the next reflow, so
/// the invalidator runs without them: a change to an element that a `:has()` argument only
/// reaches through a combinator (`.a` in `:has(.a .b)`) is assumed to change the match.
fn invalidator<'a, 'b, 'dom: 'a>(
    quirks_mode: QuirksMode,
    element: ServoDangerousStyleElement<'dom>,
    sibling_traversal_map: SiblingTraversalMap<ServoDangerousStyleElement<'dom>>,
) -> RelativeSelectorInvalidator<'a, 'b, ServoDangerousStyleElement<'dom>> {
    RelativeSelectorInvalidator {
        element,
        quirks_mode,
        snapshot_table: None,
        invalidated: note_invalidated_anchor,
        sibling_traversal_map,
        _marker: PhantomData,
    }
}

fn invalidate_for_sibling_side_effects<'dom>(
    stylist: &Stylist,
    prev_sibling: ServoDangerousStyleElement<'dom>,
    next_sibling: ServoDangerousStyleElement<'dom>,
) {
    let quirks_mode = stylist.quirks_mode();
    invalidator(
        quirks_mode,
        prev_sibling,
        SiblingTraversalMap::new(
            prev_sibling,
            prev_sibling.prev_sibling_element(),
            Some(next_sibling),
        ),
    )
    .invalidate_relative_selectors_for_dom_mutation(
        false,
        stylist,
        ElementSelectorFlags::empty(),
        DomMutationOperation::SideEffectPrevSibling,
    );
    invalidator(
        quirks_mode,
        next_sibling,
        SiblingTraversalMap::new(
            next_sibling,
            Some(prev_sibling),
            next_sibling.next_sibling_element(),
        ),
    )
    .invalidate_relative_selectors_for_dom_mutation(
        false,
        stylist,
        ElementSelectorFlags::empty(),
        DomMutationOperation::SideEffectNextSibling,
    );
}

impl Element {
    /// Runs `invalidate` against the stylist, then marks the restyled anchors for the next
    /// style traversal.
    fn invalidate_relative_selectors(
        &self,
        invalidate: impl for<'dom> FnOnce(&'dom Stylist, ServoDangerousStyleElement<'dom>),
    ) {
        let window = self.owner_window();
        {
            let layout = window.layout();
            with_layout_state(|| {
                // Borrow the live element for the layout-only traversal; no script runs here.
                let element = LayoutDom::from_raw(self);
                invalidate(layout.stylist(), ServoDangerousStyleElement::from(element))
            });
        }
        let anchors = INVALIDATED_ANCHORS.take();
        if anchors.is_empty() {
            return;
        }
        let document = self.owner_document();
        for (anchor, siblings) in anchors {
            // SAFETY: The anchors are ancestors of this element or their earlier siblings, so
            // the tree keeps them alive, and no script has run since they were noted.
            let anchor = unsafe { &*anchor }.upcast::<Node>();
            if siblings {
                let parent = anchor
                    .GetParentNode()
                    .expect("Restyled the later siblings of an element without a parent");
                document.note_node_with_invalidated_descendants(&parent);
            } else {
                document.note_node_with_invalidated_descendants(anchor);
            }
        }
    }

    /// Gecko skips elements that have no style data and that no `:has()` search visited: they
    /// are in a `display: none` subtree or outside the document, and no anchor depends on them.
    fn may_affect_relative_selectors(&self) -> bool {
        self.is_styled() || self.get_selector_flags().intersects(SEARCH_DIRECTION)
    }

    /// The search directions an element inserted here (or removed from here) takes part in.
    fn inherited_relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let node = self.upcast::<Node>();
        let mut inherited = node.GetParentElement().map_or(ElementSelectorFlags::empty(), |p| {
            p.get_selector_flags().intersection(
                ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR,
            )
        });
        if let Some(prev_sibling) = node
            .preceding_siblings()
            .find_map(DomRoot::downcast::<Element>)
        {
            // Both directions: a sibling with `:has(~ .sibling .descendant)` searches the
            // descendants of its later siblings.
            inherited |= prev_sibling
                .get_selector_flags()
                .intersection(SEARCH_DIRECTION);
        }
        inherited
    }

    /// After `changed` states of this element changed.
    pub(crate) fn invalidate_relative_selectors_for_state(&self, changed: ElementState) {
        if !self.may_affect_relative_selectors() {
            return;
        }
        self.invalidate_relative_selectors(|stylist, element| {
            invalidator(stylist.quirks_mode(), element, SiblingTraversalMap::default())
                .invalidate_relative_selectors_for_this(
                    stylist,
                    |element, scope, data, quirks_mode, collector| {
                        data.relative_selector_invalidation_map()
                            .state_affecting_selectors
                            .lookup_with_additional(
                                *element,
                                quirks_mode,
                                None,
                                &[],
                                changed,
                                |dependency| {
                                    if dependency.state.intersects(changed) {
                                        collector.add_dependency(&dependency.dep, *element, scope);
                                    }
                                    true
                                },
                            );
                    },
                );
        });
    }

    /// After the attribute `name` in `namespace` of this element changed.
    pub(crate) fn invalidate_relative_selectors_for_attribute(
        &self,
        name: &LocalName,
        namespace: &Namespace,
    ) {
        if !self.may_affect_relative_selectors() {
            return;
        }
        let is_id = *namespace == ns!() && *name == local_name!("id");
        let is_class = *namespace == ns!() && *name == local_name!("class");
        let mut old_id = None;
        let mut old_classes: Vec<Atom> = Vec::new();
        if is_id || is_class {
            let document = self.owner_document();
            let restyle = document.ensure_pending_restyle(self);
            let snapshot = restyle
                .snapshot
                .as_ref()
                .expect("Attribute changed without a snapshot");
            old_id = snapshot.id_attr().cloned();
            snapshot.each_class(|class| old_classes.push((**class).clone()));
        }
        self.invalidate_relative_selectors(|stylist, element| {
            let mut changed_classes: Vec<Atom> = Vec::new();
            if is_class {
                element.each_class(|class| {
                    if !old_classes.contains(&**class) {
                        changed_classes.push((**class).clone());
                    }
                });
                for class in &old_classes {
                    let mut kept = false;
                    element.each_class(|current| kept |= **current == *class);
                    if !kept {
                        changed_classes.push(class.clone());
                    }
                }
            }
            let new_id = element.id().cloned();
            invalidator(stylist.quirks_mode(), element, SiblingTraversalMap::default())
                .invalidate_relative_selectors_for_this(
                    stylist,
                    |element, scope, data, quirks_mode, collector| {
                        let map = data.relative_selector_invalidation_map();
                        if is_id {
                            for id in old_id.iter().chain(new_id.iter()) {
                                for dependency in map.id_to_selector.get(id, quirks_mode).into_iter().flatten() {
                                    collector.add_dependency(dependency, *element, scope);
                                }
                            }
                        }
                        for class in &changed_classes {
                            for dependency in map.class_to_selector.get(class, quirks_mode).into_iter().flatten() {
                                collector.add_dependency(dependency, *element, scope);
                            }
                        }
                        for dependency in map
                            .other_attribute_affecting_selectors
                            .get(style::LocalName::cast(name))
                            .into_iter()
                            .flatten()
                        {
                            collector.add_dependency(dependency, *element, scope);
                        }
                    },
                );
        });
    }

    /// After this element was inserted.
    pub(crate) fn invalidate_relative_selectors_for_insertion(&self) {
        // An element inserted where no search ran can't change a `:has()` match. Descendant
        // anchors don't exist yet, and later-sibling anchors of `:has()` arguments that look
        // backwards are restyled through `HAS_SLOW_SELECTOR_LATER_SIBLINGS`.
        let inherited = self.inherited_relative_selector_search_direction();
        if inherited.is_empty() {
            return;
        }
        self.invalidate_relative_selectors(|stylist, element| {
            let next_sibling = element.next_sibling_element();
            // Inserting between two siblings can break a chain through them, as in
            // `.a:has(+ .b)` or `:has(.a + .b)`.
            if let (Some(prev_sibling), Some(next_sibling)) =
                (element.prev_sibling_element(), next_sibling) &&
                prev_sibling.relative_selector_search_direction().intersects(
                    ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                )
            {
                element.apply_selector_flags(
                    ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
                );
                invalidate_for_sibling_side_effects(stylist, prev_sibling, next_sibling);
            }
            let operation = match next_sibling {
                Some(_) => DomMutationOperation::Insert,
                None => DomMutationOperation::Append,
            };
            invalidator(stylist.quirks_mode(), element, SiblingTraversalMap::default())
                .invalidate_relative_selectors_for_dom_mutation(true, stylist, inherited, operation);
        });
    }

    /// Before this element is removed: the invalidation walks up from it.
    pub(crate) fn invalidate_relative_selectors_for_removal(&self) {
        if !self.get_selector_flags().intersects(SEARCH_DIRECTION) {
            return;
        }
        let inherited = self.inherited_relative_selector_search_direction();
        if inherited.is_empty() {
            return;
        }
        self.invalidate_relative_selectors(|stylist, element| {
            if let (Some(prev_sibling), Some(next_sibling)) =
                (element.prev_sibling_element(), element.next_sibling_element())
            {
                invalidate_for_sibling_side_effects(stylist, prev_sibling, next_sibling);
            }
            invalidator(stylist.quirks_mode(), element, SiblingTraversalMap::default())
                .invalidate_relative_selectors_for_dom_mutation(
                    true,
                    stylist,
                    inherited,
                    DomMutationOperation::Remove,
                );
        });
    }

    /// After the children of this element changed, for the tree-structural pseudo-classes
    /// (`:empty`, `:first-child`, `:nth-child()`, ...) inside `:has()` arguments that matched
    /// against this element or its children. The selector flags matching left on this element
    /// say which of them were tested.
    pub(crate) fn invalidate_relative_selectors_for_child_list_change(
        &self,
        mutation: &ChildrenMutation,
        no_gc: &NoGC,
    ) {
        let flags = self.get_selector_flags();
        if flags.intersects(ElementSelectorFlags::HAS_EMPTY_SELECTOR) {
            self.invalidate_relative_selectors_for_tree_structure(TSStateForInvalidation::EMPTY);
        }
        if flags.intersects(
            ElementSelectorFlags::HAS_SLOW_SELECTOR_NTH |
                ElementSelectorFlags::HAS_SLOW_SELECTOR_NTH_OF,
        ) {
            let first_changed = if flags.intersects(ElementSelectorFlags::HAS_SLOW_SELECTOR) {
                self.upcast::<Node>().GetFirstChild()
            } else if flags.intersects(ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS) {
                mutation.next_child().map(DomRoot::from_ref)
            } else {
                None
            };
            for sibling in first_changed
                .iter()
                .flat_map(|child| child.inclusively_following_siblings())
                .filter_map(DomRoot::downcast::<Element>)
            {
                sibling.invalidate_relative_selectors_for_tree_structure(TSStateForInvalidation::NTH);
            }
        }
        if flags.intersects(ElementSelectorFlags::HAS_EDGE_CHILD_SELECTOR) &&
            let Some(edge) = mutation
                .modified_edge_element(no_gc)
                .and_then(DomRoot::downcast::<Element>)
        {
            edge.invalidate_relative_selectors_for_tree_structure(
                TSStateForInvalidation::NTH_EDGE_FIRST | TSStateForInvalidation::NTH_EDGE_LAST,
            );
        }
    }

    fn invalidate_relative_selectors_for_tree_structure(&self, state: TSStateForInvalidation) {
        self.invalidate_relative_selectors(|stylist, element| {
            invalidator(stylist.quirks_mode(), element, SiblingTraversalMap::default())
                .invalidate_relative_selectors_for_this(
                    stylist,
                    |element, scope, data, quirks_mode, collector| {
                        data.relative_invalidation_map_attributes()
                            .ts_state_to_selector
                            .lookup_with_additional(
                                *element,
                                quirks_mode,
                                None,
                                &[],
                                ElementState::empty(),
                                |dependency| {
                                    if dependency.state.intersects(state) {
                                        collector.add_dependency(&dependency.dep, *element, scope);
                                    }
                                    true
                                },
                            );
                    },
                );
        });
    }
}
