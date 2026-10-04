/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use app_units::Au;
use atomic_refcell::{AtomicRef, AtomicRefCell};
use rustc_hash::FxHashMap;
use euclid::Point2D;
use layout_api::LayoutDamage;
use malloc_size_of_derive::MallocSizeOf;
use servo_arc::Arc as ServoArc;
use style::Zero;
use style::computed_values::position::T as Position;
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;
use style::values::computed::ContainerType;
use style::values::specified::align::AlignFlags;
use style_traits::CSSPixel;

use crate::context::LayoutContext;
use crate::dom::{LayoutBox, WeakLayoutBox};
use crate::flow::CollapsibleWithParentStartMargin;
use crate::formatting_contexts::Baselines;
use crate::fragment_tree::{
    BaseFragmentInfo, BoxFragment, CollapsedBlockMargins, Fragment, FragmentStatus,
    SpecificLayoutInfo,
};
use crate::geom::LogicalSides1D;
use crate::positioned::{PositioningContext, relative_adjustement};
use crate::sizing::{
    ComputeInlineContentSizes, ContentSizes, InlineContentSizesResult, LazySizeKind, SizeConstraint,
};
use crate::traversal::ElementDamageSet;
use crate::{ConstraintSpace, ContainingBlock, ContainingBlockSize};

/// A box tree node that handles containing information about style and the original DOM
/// node or pseudo-element that it is based on. This also handles caching of layout values
/// such as the inline content sizes to avoid recalculating these values during layout
/// passes.
///
/// In the future, this will hold layout results to support incremental layout.
#[derive(MallocSizeOf)]
pub(crate) struct LayoutBoxBase {
    pub base_fragment_info: BaseFragmentInfo,
    pub style: ServoArc<ComputedValues>,
    pub cached_inline_content_size:
        AtomicRefCell<Option<Box<(SizeConstraint, Option<Au>, InlineContentSizesResult)>>>,
    pub outer_inline_content_sizes_depend_on_content: AtomicBool,

    /// The cached layout results for this [`LayoutBoxBase`]. These are either cached
    /// independent formatting context results or a cached block layout for use within
    /// a block flow.
    cached_layout_result: AtomicRefCell<Option<LayoutResultAndInputs>>,

    /// The cached independent formatting context layout whose block size was left to its
    /// contents (see [`LazySizeKind`]). Flex layout lays an item out
    /// once to measure it and once more at its final size; keeping the measurement apart
    /// from [`Self::cached_layout_result`] stops the two from evicting each other, which made
    /// nested flex containers take time exponential in their depth.
    cached_measure_result:
        AtomicRefCell<Option<Box<IndependentFormattingContextLayoutResultAndInputs>>>,

    /// Whether or not the cached layout result for this [`LayoutBoxBase`] is dirty.
    /// This flag is used to preserve the cache when it can be used to do a faster
    /// layout, but cannot be reused directly.
    cached_layout_result_dirty: AtomicBool,

    /// A count of the number of boxes are in this box's subtree (including itself).
    /// This is used as a heuristic to know when to perform parallel layout.
    subtree_size: AtomicUsize,

    #[conditional_malloc_size_of]
    fragments: Arc<BoxFragments>,
    pub parent_box: Option<WeakLayoutBox>,
}

/// The fragments of a box. Each of them refers back to this, so that reusing a cached layout
/// result can make the fragments in it the fragments of their boxes again: in the meantime the
/// boxes may have been laid out at another size, and layout queries would otherwise look at
/// fragments that are not in the fragment tree.
#[derive(Default, MallocSizeOf)]
pub(crate) struct BoxFragments {
    fragments: AtomicRefCell<Vec<Fragment>>,
    /// Which of the cached layout results of the box the fragments of its descendants belong to:
    /// one of the `CONTENTS_FROM_*` values.
    contents_from: AtomicU8,
}

const CONTENTS_FROM_UNKNOWN: u8 = 0;
const CONTENTS_FROM_LAYOUT: u8 = 1;
const CONTENTS_FROM_MEASURE: u8 = 2;

/// Make `fragments` and their descendants the fragments of the boxes they were made for.
fn make_fragments_current(fragments: &[Fragment]) {
    type FragmentsByOwner = FxHashMap<*const BoxFragments, (Arc<BoxFragments>, Vec<Fragment>)>;
    fn collect(fragment: &Fragment, by_owner: &mut FragmentsByOwner) {
        if let Some(owner) = fragment
            .base()
            .and_then(|base| base.owner.borrow().as_ref().and_then(Weak::upgrade))
        {
            by_owner
                .entry(Arc::as_ptr(&owner))
                .or_insert_with(|| (owner.clone(), Vec::new()))
                .1
                .push(fragment.clone());
        }
        match fragment {
            Fragment::LayoutRoot(layout_root_fragment) => {
                collect(&layout_root_fragment.inner(), by_owner)
            },
            Fragment::Box(box_fragment) | Fragment::Float(box_fragment) => {
                for child in &box_fragment.children {
                    collect(child, by_owner);
                }
            },
            Fragment::Positioning(positioning_fragment) => {
                for child in &positioning_fragment.children {
                    collect(child, by_owner);
                }
            },
            Fragment::Text(_) |
            Fragment::AbsoluteOrFixedPositionedPlaceholder(_) |
            Fragment::Image(_) |
            Fragment::IFrame(_) => {},
        }
    }

    let mut by_owner = FxHashMap::default();
    for fragment in fragments {
        collect(fragment, &mut by_owner);
    }
    for (owner, fragments) in by_owner.into_values() {
        *owner.fragments.borrow_mut() = fragments;
        // The descendants of this box now hold the fragments of one of its earlier layouts,
        // which might not be the one its cache slots remember as current.
        owner
            .contents_from
            .store(CONTENTS_FROM_UNKNOWN, Ordering::Relaxed);
    }
}

impl LayoutBoxBase {
    pub(crate) fn new(
        base_fragment_info: BaseFragmentInfo,
        style: ServoArc<ComputedValues>,
    ) -> Self {
        Self {
            base_fragment_info,
            style,
            cached_inline_content_size: AtomicRefCell::default(),
            outer_inline_content_sizes_depend_on_content: AtomicBool::new(true),
            cached_layout_result: AtomicRefCell::default(),
            cached_measure_result: AtomicRefCell::default(),
            cached_layout_result_dirty: AtomicBool::default(),
            subtree_size: AtomicUsize::default(),
            fragments: Arc::default(),
            parent_box: None,
        }
    }

    /// Set the subtree size on this [`LayoutBoxBase`]. This should be done once
    /// box construction knows how many boxes are in this box's subtree.
    pub(crate) fn set_subtree_size(&self, size: usize) {
        self.subtree_size.store(size, Ordering::Relaxed);
    }

    pub(crate) fn subtree_size(&self) -> usize {
        self.subtree_size.load(Ordering::Relaxed)
    }

    /// Get the inline content sizes of a box tree node that extends this [`LayoutBoxBase`], fetch
    /// the result from a cache when possible.
    pub(crate) fn inline_content_sizes(
        &self,
        layout_context: &LayoutContext,
        constraint_space: &ConstraintSpace,
        layout_box: &impl ComputeInlineContentSizes,
    ) -> InlineContentSizesResult {
        let mut cache = self.cached_inline_content_size.borrow_mut();
        if let Some(cached_inline_content_size) = cache.as_ref() {
            let (previous_cb_block_size, previous_replaced_percentage_block_size, result) =
                **cached_inline_content_size;
            if !result.depends_on_block_constraints ||
                (previous_cb_block_size == constraint_space.block_size &&
                    previous_replaced_percentage_block_size ==
                        constraint_space.replaced_percentage_block_size)
            {
                return result;
            }
            // TODO: Should we keep multiple caches for various block sizes?
        }

        // <https://drafts.csswg.org/css-conditional-5/#container-type>: a size container has
        // inline-size containment, so its intrinsic inline size is that of an empty box.
        let result = if self.style.clone_container_type().intersects(
            ContainerType::INLINE_SIZE | ContainerType::SIZE,
        ) {
            InlineContentSizesResult {
                sizes: ContentSizes::zero(),
                depends_on_block_constraints: false,
            }
        } else {
            layout_box.compute_inline_content_sizes_with_fixup(layout_context, constraint_space)
        };
        *cache = Some(Box::new((
            constraint_space.block_size,
            constraint_space.replaced_percentage_block_size,
            result,
        )));
        result
    }

    pub(crate) fn fragments(&self) -> AtomicRef<'_, Vec<Fragment>> {
        self.fragments.fragments.borrow()
    }

    fn claim_fragment(&self, fragment: &Fragment) {
        if let Some(base) = fragment.base() {
            *base.owner.borrow_mut() = Some(Arc::downgrade(&self.fragments));
        }
    }

    pub(crate) fn add_fragment(&self, fragment: Fragment) {
        self.claim_fragment(&fragment);
        self.fragments.fragments.borrow_mut().push(fragment);
    }

    pub(crate) fn set_fragment(&self, fragment: Fragment) {
        self.claim_fragment(&fragment);
        *self.fragments.fragments.borrow_mut() = vec![fragment];
    }

    pub(crate) fn clear_fragments(&self) {
        self.fragments.fragments.borrow_mut().clear();
    }

    /// Record that the fragments of the descendants of this box belong to the cached layout
    /// result in `contents_from`, making them so first if they don't.
    fn use_contents_from(&self, contents_from: u8, fragments: &[Fragment]) {
        if self.fragments.contents_from.load(Ordering::Relaxed) != contents_from {
            make_fragments_current(fragments);
            self.fragments
                .contents_from
                .store(contents_from, Ordering::Relaxed);
        }
    }

    /// Clear all resulting fragments and dirty and fragment caches. Resulting fragments are
    /// used for layout queries and fragment caches are used for incremental layout.
    pub(crate) fn clear_fragments_and_dirty_fragment_cache(&self) {
        self.clear_fragments();
        self.cached_layout_result_dirty
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn repair_style(&mut self, new_style: &ServoArc<ComputedValues>) {
        self.style = new_style.clone();
        for fragment in self.fragments.fragments.borrow_mut().iter_mut() {
            if let Some(base) = fragment.base() {
                base.repair_style(new_style);
            }
        }
    }

    #[expect(unused)]
    pub(crate) fn parent_box(&self) -> Option<LayoutBox> {
        self.parent_box.as_ref().and_then(WeakLayoutBox::upgrade)
    }

    /// Clear fragment layout caches on this base, depending on upward flowing damage, but
    /// *do not* clear its resulting fragment. The layout cache itself is always cleared,
    /// but the inline content size cache is cleared conditionally.
    ///
    /// Returns true is this [`LayoutBoxBase`] propagates `RecomputeInlineContentSizes`
    /// and false otherwise.
    pub(crate) fn invalidate_caches(&self, damage_set: &ElementDamageSet) -> bool {
        self.cached_layout_result_dirty
            .store(true, Ordering::Relaxed);
        if !damage_set.on_element.is_empty() ||
            damage_set
                .from_children
                .contains(LayoutDamage::RecomputeInlineContentSizes)
        {
            *self.cached_inline_content_size.borrow_mut() = None;
        }

        // When a block container has a mix of inline-level and block-level contents, the
        // inline-level ones are wrapped inside an anonymous block associated with the
        // block container. The anonymous block has an `auto` size, so its intrinsic
        // contribution depends on content, but it can't affect the intrinsic size of
        // ancestors if the block container is sized extrinsically.
        //
        // If the intrinsic contributions of this node depend on content, we will need to
        // clear the cached intrinsic sizes of the parent. But if the contributions are
        // purely extrinsic, then the intrinsic sizes of the ancestors won't be affected,
        // and we can keep the cache.
        !self.base_fragment_info.is_anonymous() &&
            self.outer_inline_content_sizes_depend_on_content
                .load(Ordering::Relaxed)
    }

    /// Clear fragment layout caches on this base, depending on upward flowing damage, and
    /// also clear its resulting fragment. The layout cache itself is always cleared, but
    /// the inline content size cache is cleared conditionally.
    ///
    /// Returns true is this [`LayoutBoxBase`] propagates `RecomputeInlineContentSizes`
    /// and false otherwise.
    pub(crate) fn invalidate_caches_for_fragment_tree_layout(
        &self,
        damage_set: &ElementDamageSet,
    ) -> bool {
        self.clear_fragments();
        self.invalidate_caches(damage_set)
    }

    pub(crate) fn cached_independent_formatting_context_layout_if_applicable(
        &self,
        positioning_context: &mut PositioningContext,
        containing_block_for_children: &ContainingBlock<'_>,
        lazy_block_size: LazySizeKind,
    ) -> Option<IndependentFormattingContextLayoutResult> {
        if self.cached_layout_result_dirty.load(Ordering::Relaxed) {
            return None;
        }

        let applies = |cache: &IndependentFormattingContextLayoutResultAndInputs| {
            cache.containing_block_for_children_size.inline ==
                containing_block_for_children.size.inline &&
                (cache.containing_block_for_children_size == containing_block_for_children.size ||
                    !cache.result.depends_on_block_constraints)
        };

        let layout_cache = self.cached_layout_result.borrow();
        let measure_cache = self.cached_measure_result.borrow();
        let measure = measure_cache.as_deref().filter(|cache| applies(cache));
        let measure =
            |filter: &dyn Fn(&IndependentFormattingContextLayoutResultAndInputs) -> bool| {
                measure
                    .filter(|cache| filter(cache))
                    .map(|cache| (cache, CONTENTS_FROM_MEASURE))
            };
        let (cache, contents_from) = match lazy_block_size {
            LazySizeKind::Fixed(block_size) => {
                let layout = match &*layout_cache {
                    Some(LayoutResultAndInputs::IndependentFormattingContext(cache))
                        if cache.lazy_block_size == lazy_block_size && applies(cache) =>
                    {
                        Some((&**cache, CONTENTS_FROM_LAYOUT))
                    },
                    _ => None,
                };
                // Being told to use the block size that the contents asked for anyway gives
                // the same layout as measuring, which is what a flex container does with
                // its items while it is itself being measured.
                layout.or_else(|| {
                    measure(&|cache| {
                        cache.lazy_block_size == LazySizeKind::Intrinsic &&
                            cache.result.content_block_size == block_size
                    })
                })
            },
            LazySizeKind::Intrinsic | LazySizeKind::Constrained => {
                measure(&|cache| cache.lazy_block_size == lazy_block_size)
            },
        }?;

        self.use_contents_from(contents_from, &cache.result.fragments);
        positioning_context.append(cache.positioning_context.clone());
        Some(cache.result.clone())
    }

    pub(crate) fn cache_independent_formatting_context_layout(
        &self,
        containing_block_for_children: &ContainingBlock<'_>,
        lazy_block_size: LazySizeKind,
        child_positioning_context: &PositioningContext,
        result: &IndependentFormattingContextLayoutResult,
    ) {
        let was_dirty = self
            .cached_layout_result_dirty
            .swap(false, Ordering::Relaxed);
        let entry = Box::new(IndependentFormattingContextLayoutResultAndInputs {
            result: result.clone(),
            positioning_context: child_positioning_context.clone(),
            containing_block_for_children_size: containing_block_for_children.size.clone(),
            lazy_block_size,
        });
        // Clearing the dirty flag revalidates both slots, so the one not written here must go.
        let contents_from = match lazy_block_size {
            LazySizeKind::Fixed(_) => CONTENTS_FROM_LAYOUT,
            LazySizeKind::Intrinsic | LazySizeKind::Constrained => CONTENTS_FROM_MEASURE,
        };
        self.fragments
            .contents_from
            .store(contents_from, Ordering::Relaxed);
        match lazy_block_size {
            LazySizeKind::Fixed(_) => {
                if was_dirty {
                    *self.cached_measure_result.borrow_mut() = None;
                }
                *self.cached_layout_result.borrow_mut() =
                    Some(LayoutResultAndInputs::IndependentFormattingContext(entry));
            },
            LazySizeKind::Intrinsic | LazySizeKind::Constrained => {
                if was_dirty {
                    *self.cached_layout_result.borrow_mut() = None;
                }
                *self.cached_measure_result.borrow_mut() = Some(entry);
            },
        }
    }

    pub(crate) fn cached_same_formatting_context_block_if_applicable(
        &self,
        containing_block: &ContainingBlock,
        collapsible_with_parent_start_margin: Option<CollapsibleWithParentStartMargin>,
        ignore_block_margins_for_stretch: LogicalSides1D<bool>,
        has_inline_parent: bool,
    ) -> Option<Arc<BoxFragment>> {
        if self.cached_layout_result_dirty.load(Ordering::Relaxed) {
            return None;
        }

        let mut cached_layout_result = self.cached_layout_result.borrow_mut();
        let Some(LayoutResultAndInputs::SameFormattingContextBlock(result)) =
            &mut *cached_layout_result
        else {
            return None;
        };

        if result.containing_block_size != containing_block.size ||
            result.containing_block_writing_mode != containing_block.style.writing_mode ||
            result.containing_block_justify_items !=
                containing_block.style.clone_justify_items().computed.0.0 ||
            result.collapsible_with_parent_start_margin != collapsible_with_parent_start_margin ||
            result.ignore_block_margins_for_stretch != ignore_block_margins_for_stretch ||
            result.has_inline_parent != has_inline_parent
        {
            return None;
        }

        let fragment = result.result.fragment.clone();
        self.use_contents_from(CONTENTS_FROM_LAYOUT, &fragment.children);
        {
            let mut origin = result.result.original_offset;
            if self.style.clone_position() == Position::Relative {
                origin += relative_adjustement(&self.style, containing_block)
                    .to_physical_vector(containing_block.style.writing_mode)
            }
            fragment.base.set_rect_origin(origin);
        }

        Some(fragment)
    }

    pub(crate) fn cache_same_formatting_context_block_layout(
        &self,
        containing_block: &ContainingBlock,
        collapsible_with_parent_start_margin: Option<CollapsibleWithParentStartMargin>,
        ignore_block_margins_for_stretch: LogicalSides1D<bool>,
        has_inline_parent: bool,
        fragment: Arc<BoxFragment>,
    ) {
        let mut original_offset;
        {
            original_offset = fragment.content_rect().origin;
            if self.style.clone_position() == Position::Relative {
                original_offset -= relative_adjustement(&self.style, containing_block)
                    .to_physical_vector(containing_block.style.writing_mode)
            }
        }

        if self
            .cached_layout_result_dirty
            .swap(false, Ordering::Relaxed)
        {
            *self.cached_measure_result.borrow_mut() = None;
        }
        self.fragments
            .contents_from
            .store(CONTENTS_FROM_LAYOUT, Ordering::Relaxed);
        *self.cached_layout_result.borrow_mut() =
            Some(LayoutResultAndInputs::SameFormattingContextBlock(Box::new(
                SameFormattingContextBlockLayoutResultAndInputs {
                    result: SameFormattingContextBlockLayoutResult {
                        fragment,
                        original_offset,
                    },
                    containing_block_size: containing_block.size.clone(),
                    containing_block_writing_mode: containing_block.style.writing_mode,
                    containing_block_justify_items: containing_block
                        .style
                        .clone_justify_items()
                        .computed
                        .0
                        .0,
                    collapsible_with_parent_start_margin,
                    ignore_block_margins_for_stretch,
                    has_inline_parent,
                },
            )));
    }

    pub(crate) fn clear_scrollable_overflow_all_on_fragments(&self) {
        for fragment in self.fragments().iter() {
            fragment.clear_scrollable_overflow();
        }
    }

    pub(crate) fn mark_fragments_as_descendants_changed(&self) {
        for fragment in self.fragments().iter() {
            if let Some(base) = fragment.base() {
                base.set_status(FragmentStatus::OnlyDescendantsChanged);
            }
        }
    }
}

impl Debug for LayoutBoxBase {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("LayoutBoxBase").finish()
    }
}

#[derive(MallocSizeOf)]
pub(crate) enum LayoutResultAndInputs {
    IndependentFormattingContext(Box<IndependentFormattingContextLayoutResultAndInputs>),
    SameFormattingContextBlock(Box<SameFormattingContextBlockLayoutResultAndInputs>),
}

#[derive(Clone, MallocSizeOf)]
pub(crate) struct IndependentFormattingContextLayoutResult {
    pub fragments: Vec<Fragment>,

    /// <https://drafts.csswg.org/css2/visudet.html#root-height>
    pub content_block_size: Au,

    /// If this layout is for a block container, this tracks the collapsable size
    /// of start and end margins and whether or not the block container collapsed through.
    pub collapsible_margins_in_children: CollapsedBlockMargins,

    /// The contents of a table may force it to become wider than what we would expect
    /// from 'width' and 'min-width'. This is the resulting inline content size,
    /// or None for non-table layouts.
    pub content_inline_size_for_table: Option<Au>,

    /// The offset of the last inflow baseline of this layout in the content area, if
    /// there was one. This is used to propagate baselines to the ancestors of `display:
    /// inline-block`.
    pub baselines: Baselines,

    /// Whether or not this layout depends on the containing block size.
    pub depends_on_block_constraints: bool,

    /// Additional information of this layout that could be used by Javascripts and devtools.
    pub specific_layout_info: Option<SpecificLayoutInfo>,
}

/// A collection of layout inputs and a cached layout result for an IndependentFormattingContext for
/// use in [`LayoutBoxBase`].
#[derive(MallocSizeOf)]
pub(crate) struct IndependentFormattingContextLayoutResultAndInputs {
    /// The [`IndependentFormattingContextLayoutResult`] for this layout.
    pub result: IndependentFormattingContextLayoutResult,

    /// The [`ContainingBlockSize`] to use for this box's contents, but not
    /// for the box itself.
    pub containing_block_for_children_size: ContainingBlockSize,

    /// How the block size of this layout was determined. The result depends on it even when
    /// the containing block size is indefinite in the block axis, as for a column flex item
    /// whose main size is not definite: that layout reports the used main size as its content
    /// block size, which must not be reused when the intrinsic block size is requested.
    pub lazy_block_size: LazySizeKind,

    /// A [`PositioningContext`] holding absolutely-positioned descendants
    /// collected during the layout of this box.
    pub positioning_context: PositioningContext,
}

#[derive(Clone, MallocSizeOf)]
pub(crate) struct SameFormattingContextBlockLayoutResult {
    #[conditional_malloc_size_of]
    pub fragment: Arc<BoxFragment>,
    original_offset: Point2D<Au, CSSPixel>,
}

/// A collection of layout inputs and a cached layout result for a SameFormattingContextBlock for
/// use in [`LayoutBoxBase`].
#[derive(MallocSizeOf)]
pub(crate) struct SameFormattingContextBlockLayoutResultAndInputs {
    pub result: SameFormattingContextBlockLayoutResult,
    /// The [`ContainingBlockSize`] used when this block was laid out.
    pub containing_block_size: ContainingBlockSize,
    /// The containing block's [`WritingMode`]  used when this block was laid out.
    pub containing_block_writing_mode: WritingMode,
    /// The containing block's `justify-items` [`AlignFlags`] used when this block was laid out.
    pub containing_block_justify_items: AlignFlags,
    /// Whether or not the margin in this block was collapsible with the parent's start margin
    /// when this block was laid out.
    collapsible_with_parent_start_margin: Option<CollapsibleWithParentStartMargin>,
    /// Whether or not block margins were ignored for stretch when this block was laid out.
    ignore_block_margins_for_stretch: LogicalSides1D<bool>,
    /// Whether or not this block had an inline parent.
    has_inline_parent: bool,
}
