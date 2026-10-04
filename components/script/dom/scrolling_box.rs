/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use app_units::Au;
use euclid::{Rect, SideOffsets2D, Vector2D};
use js::context::JSContext;
use layout_api::{AxesOverflow, ScrollContainerQueryFlags};
use script_bindings::codegen::GenericBindings::WindowBinding::ScrollBehavior;
use script_bindings::inheritance::Castable;
use script_bindings::root::DomRoot;
use style::values::computed::{
    Length, NonNegativeLengthPercentageOrAuto, Overflow, ScrollSnapAxis, ScrollSnapStop,
    ScrollSnapStrictness,
};
use style::values::generics::length::LengthPercentageOrAuto;
use style::values::specified::box_::ScrollSnapAlignKeyword;
use style_traits::CSSPixel;
use webrender_api::units::{LayoutPixel, LayoutSize, LayoutVector2D};

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::ElementBinding::ScrollLogicalPosition;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::types::{Document, Element};

pub(crate) struct ScrollingBox {
    target: ScrollingBoxSource,
    overflow: AxesOverflow,
    cached_content_size: Cell<Option<LayoutSize>>,
    cached_size: Cell<Option<LayoutSize>>,
}

/// Represents a scrolling box that can be either an element or the viewport
/// <https://drafts.csswg.org/cssom-view/#scrolling-box>
pub(crate) enum ScrollingBoxSource {
    Element(DomRoot<Element>),
    Viewport(DomRoot<Document>),
}

/// What choosing a snap position of a snap container depends on.
struct SnapContext {
    snaps_x: bool,
    snaps_y: bool,
    /// How far from where a scroll would otherwise end a `proximity` snap container still
    /// snaps, or `None` for a `mandatory` one.
    proximity_range: Option<LayoutSize>,
    scroll_padding: SideOffsets2D<f32, LayoutPixel>,
    size: LayoutSize,
    max_position: LayoutVector2D,
    scrolling_area_origin: LayoutVector2D,
    device_pixel_ratio: f32,
}

/// A snap area of a snap container and the scroll positions that snap to it.
struct SnapCandidate {
    element: DomRoot<Element>,
    x: Option<f32>,
    y: Option<f32>,
    stop_always: bool,
}

#[derive(Copy, Clone)]
pub(crate) enum ScrollingBoxAxis {
    X,
    Y,
}

#[derive(Copy, Clone)]
pub(crate) enum ScrollRequirement {
    Always,
    IfNotVisible,
}

impl ScrollRequirement {
    fn compute_need_scroll(
        &self,
        element_start: f32,
        element_end: f32,
        scrollport_start: f32,
        scrollport_end: f32,
    ) -> bool {
        match self {
            ScrollRequirement::Always => true,
            ScrollRequirement::IfNotVisible => {
                element_end <= scrollport_start || element_start >= scrollport_end
            },
        }
    }
}

#[derive(Copy, Clone)]
pub(crate) struct ScrollAxisState {
    pub(crate) position: ScrollLogicalPosition,
    pub(crate) requirement: ScrollRequirement,
}

impl ScrollAxisState {
    pub fn new_always_scroll_position(position: ScrollLogicalPosition) -> Self {
        ScrollAxisState {
            position,
            requirement: ScrollRequirement::Always,
        }
    }
}

impl ScrollingBox {
    pub(crate) fn new(target: ScrollingBoxSource, overflow: AxesOverflow) -> Self {
        Self {
            target,
            overflow,
            cached_content_size: Default::default(),
            cached_size: Default::default(),
        }
    }

    pub(crate) fn target(&self) -> &ScrollingBoxSource {
        &self.target
    }

    pub(crate) fn is_viewport(&self) -> bool {
        matches!(self.target, ScrollingBoxSource::Viewport(..))
    }

    pub(crate) fn scroll_position(&self) -> LayoutVector2D {
        match &self.target {
            ScrollingBoxSource::Element(element) => element
                .owner_window()
                .scroll_offset_query(element.upcast::<Node>()),
            ScrollingBoxSource::Viewport(document) => document.window().scroll_offset(),
        }
    }

    pub(crate) fn content_size(&self) -> LayoutSize {
        if let Some(content_size) = self.cached_content_size.get() {
            return content_size;
        }

        let (document, node_to_query) = match &self.target {
            ScrollingBoxSource::Element(element) => {
                (element.owner_document(), Some(element.upcast()))
            },
            ScrollingBoxSource::Viewport(document) => (document.clone(), None),
        };

        let content_size = document
            .window()
            .scrolling_area_query(node_to_query)
            .size
            .to_f32()
            .cast_unit();
        self.cached_content_size.set(Some(content_size));
        content_size
    }

    pub(crate) fn size(&self) -> LayoutSize {
        if let Some(size) = self.cached_size.get() {
            return size;
        }

        let size = match &self.target {
            ScrollingBoxSource::Element(element) => element.client_rect().size.to_f32().cast_unit(),
            ScrollingBoxSource::Viewport(document) => {
                document.window().viewport_details().size.cast_unit()
            },
        };
        self.cached_size.set(Some(size));
        size
    }

    pub(crate) fn parent(&self) -> Option<ScrollingBox> {
        match &self.target {
            ScrollingBoxSource::Element(element) => {
                element.scrolling_box(ScrollContainerQueryFlags::empty())
            },
            ScrollingBoxSource::Viewport(_) => None,
        }
    }

    pub(crate) fn node(&self) -> &Node {
        match &self.target {
            ScrollingBoxSource::Element(element) => element.upcast(),
            ScrollingBoxSource::Viewport(document) => document.upcast(),
        }
    }

    pub(crate) fn scroll_to(
        &self,
        cx: &mut JSContext,
        position: LayoutVector2D,
        behavior: ScrollBehavior,
    ) {
        match &self.target {
            ScrollingBoxSource::Element(element) => {
                element
                    .owner_window()
                    .scroll_an_element(cx, element, position.x, position.y, behavior, None);
            },
            ScrollingBoxSource::Viewport(document) => {
                document
                    .window()
                    .scroll(cx, position.x, position.y, behavior, None);
            },
        }
    }

    pub(crate) fn has_ongoing_smooth_scroll(&self) -> bool {
        match &self.target {
            ScrollingBoxSource::Element(element) => {
                let window = element.owner_window();
                window.has_ongoing_smooth_scroll(window.scroll_id_for_element(element))
            },
            ScrollingBoxSource::Viewport(document) => {
                let window = document.window();
                window.has_ongoing_smooth_scroll(window.pipeline_id().root_scroll_id())
            },
        }
    }

    pub(crate) fn can_keyboard_scroll_in_axis(&self, axis: ScrollingBoxAxis) -> bool {
        let overflow = match axis {
            ScrollingBoxAxis::X => self.overflow.x,
            ScrollingBoxAxis::Y => self.overflow.y,
        };
        if overflow == Overflow::Hidden {
            return false;
        }
        match axis {
            ScrollingBoxAxis::X => self.content_size().width > self.size().width,
            ScrollingBoxAxis::Y => self.content_size().height > self.size().height,
        }
    }

    /// <https://drafts.csswg.org/cssom-view/#determine-the-scroll-into-view-position>
    pub(crate) fn determine_scroll_into_view_position(
        &self,
        block: ScrollAxisState,
        inline: ScrollAxisState,
        target_rect: Rect<Au, CSSPixel>,
    ) -> LayoutVector2D {
        let device_pixel_ratio = self.node().owner_window().device_pixel_ratio().get();
        let to_pixel = |value: Au| value.to_nearest_pixel(device_pixel_ratio);

        // Step 1 should be handled by the caller, and provided as |target_rect|.
        // > Let target bounding border box be the box represented by the return value
        // > of invoking Element’s getBoundingClientRect(), if target is an Element,
        // > or Range’s getBoundingClientRect(), if target is a Range.
        let target_top_left = target_rect.origin.map(to_pixel);
        let target_bottom_right = target_rect.max().map(to_pixel);

        // The rest of the steps diverge from the specification here, but essentially try
        // to follow it using our own geometry types.
        //
        // TODO: This makes the code below wrong for the purposes of writing modes.
        let (adjusted_element_top_left, adjusted_element_bottom_right) = match self.target() {
            ScrollingBoxSource::Viewport(_) => (target_top_left, target_bottom_right),
            ScrollingBoxSource::Element(scrolling_element) => {
                let scrolling_padding_rect_top_left = scrolling_element
                    .upcast::<Node>()
                    .padding_box()
                    .unwrap_or_default()
                    .origin
                    .map(to_pixel);
                (
                    target_top_left - scrolling_padding_rect_top_left.to_vector(),
                    target_bottom_right - scrolling_padding_rect_top_left.to_vector(),
                )
            },
        };

        let size = self.size();
        let scroll_padding = self.scroll_padding(size);
        let current_scroll_position = self.scroll_position();
        Vector2D::new(
            Self::calculate_scroll_position_one_axis(
                inline,
                adjusted_element_top_left.x,
                adjusted_element_bottom_right.x,
                scroll_padding.left,
                size.width - scroll_padding.right,
                current_scroll_position.x,
            ),
            Self::calculate_scroll_position_one_axis(
                block,
                adjusted_element_top_left.y,
                adjusted_element_bottom_right.y,
                scroll_padding.top,
                size.height - scroll_padding.bottom,
                current_scroll_position.y,
            ),
        )
    }

    /// The insets of the optimal viewing region from the scrollport edges. `auto` resolves to
    /// zero, as in other engines.
    /// <https://drafts.csswg.org/css-scroll-snap-1/#scroll-padding>
    fn scroll_padding(&self, scrollport_size: LayoutSize) -> SideOffsets2D<f32, LayoutPixel> {
        let Some(style) = self
            .scroll_property_element()
            .and_then(|element| element.style())
        else {
            return SideOffsets2D::zero();
        };
        let padding = style.get_padding();
        let resolve = |value: &NonNegativeLengthPercentageOrAuto, basis: f32| match value {
            LengthPercentageOrAuto::Auto => 0.,
            LengthPercentageOrAuto::LengthPercentage(length) => {
                length.0.resolve(Length::new(basis)).px()
            },
        };
        SideOffsets2D::new(
            resolve(&padding.scroll_padding_top, scrollport_size.height),
            resolve(&padding.scroll_padding_right, scrollport_size.width),
            resolve(&padding.scroll_padding_bottom, scrollport_size.height),
            resolve(&padding.scroll_padding_left, scrollport_size.width),
        )
    }

    /// The element whose `scroll-padding` and `scroll-snap-type` apply to this box: the
    /// viewport takes them from the root element.
    fn scroll_property_element(&self) -> Option<DomRoot<Element>> {
        match &self.target {
            ScrollingBoxSource::Element(element) => Some(element.clone()),
            ScrollingBoxSource::Viewport(document) => document.GetDocumentElement(),
        }
    }

    pub(crate) fn is_snap_container(&self) -> bool {
        self.scroll_property_element()
            .is_some_and(|element| element.scroll_snap_type().is_some())
    }

    /// The scroll position a scroll intending to end at `intended` must end at instead so that
    /// this box rests on a snap position, or `intended` when this box does not snap there.
    /// `origin` is where a directional scroll (keyboard, wheel, `scrollBy()`) started: such a
    /// scroll moves on to a snap position past `origin` in its direction, the closest one to
    /// `intended`, instead of falling back to where it started, and stops at any snap area with
    /// `scroll-snap-stop: always` on the way. Without an origin the closest snap position wins.
    /// The chosen snap targets are remembered so that the box can re-snap to them when layout
    /// changes.
    /// <https://drafts.csswg.org/css-scroll-snap-1/#choosing>
    pub(crate) fn snapped_position(
        &self,
        intended: LayoutVector2D,
        origin: Option<LayoutVector2D>,
    ) -> LayoutVector2D {
        let Some(context) = self.snap_context() else {
            return intended;
        };
        let candidates = self.snap_candidates(&context);
        let x = context
            .snaps_x
            .then(|| {
                Self::choose_snap_target(
                    &candidates,
                    |candidate| candidate.x,
                    intended.x,
                    origin.map(|origin| origin.x),
                    context.proximity_range.map(|range| range.width),
                )
            })
            .flatten();
        let y = context
            .snaps_y
            .then(|| {
                Self::choose_snap_target(
                    &candidates,
                    |candidate| candidate.y,
                    intended.y,
                    origin.map(|origin| origin.y),
                    context.proximity_range.map(|range| range.height),
                )
            })
            .flatten();

        self.node().owner_doc().set_snap_targets(
            self.node(),
            x.map(|(index, _)| &*candidates[index].element),
            y.map(|(index, _)| &*candidates[index].element),
        );
        Vector2D::new(
            x.map_or(intended.x, |(_, position)| position),
            y.map_or(intended.y, |(_, position)| position),
        )
    }

    /// The scroll position that keeps this box snapped to the snap targets it last snapped to
    /// after their layout changed. When one of them is no longer a snap area of this box, the
    /// box snaps anew from its current position.
    /// <https://drafts.csswg.org/css-scroll-snap-1/#re-snap>
    pub(crate) fn resnap_position(
        &self,
        x_target: Option<&Element>,
        y_target: Option<&Element>,
    ) -> LayoutVector2D {
        let current = self.scroll_position();
        let Some(context) = self.snap_context() else {
            return current;
        };
        let target_position =
            |target: Option<&Element>, axis: fn(&SnapCandidate) -> Option<f32>| {
                target.map(|target| {
                    self.snap_candidate(&context, target)
                        .as_ref()
                        .and_then(axis)
                })
            };
        match (
            target_position(x_target, |candidate| candidate.x),
            target_position(y_target, |candidate| candidate.y),
        ) {
            (Some(None), _) | (_, Some(None)) => self.snapped_position(current, None),
            (x, y) => Vector2D::new(
                x.flatten().unwrap_or(current.x),
                y.flatten().unwrap_or(current.y),
            ),
        }
    }

    fn snap_context(&self) -> Option<SnapContext> {
        let snap_type = self.scroll_property_element()?.scroll_snap_type()?;
        // TODO: Map the logical axes through the writing mode instead of assuming
        // horizontal-tb.
        let (snaps_x, snaps_y) = match snap_type.axis {
            ScrollSnapAxis::X | ScrollSnapAxis::Inline => (true, false),
            ScrollSnapAxis::Y | ScrollSnapAxis::Block => (false, true),
            ScrollSnapAxis::Both => (true, true),
        };

        let size = self.size();
        let content_size = self.content_size();
        let scroll_position = self.scroll_position();
        // Snap areas come in the coordinate space of `getBoundingClientRect()`. Moving them
        // into this box's scrolling area makes their positions independent of the current
        // scroll position.
        let scrolling_area_origin = match &self.target {
            ScrollingBoxSource::Viewport(_) => -scroll_position,
            ScrollingBoxSource::Element(element) => {
                let padding_box_origin = element
                    .upcast::<Node>()
                    .padding_box()
                    .unwrap_or_default()
                    .origin;
                Vector2D::new(
                    padding_box_origin.x.to_f32_px(),
                    padding_box_origin.y.to_f32_px(),
                ) - scroll_position
            },
        };
        Some(SnapContext {
            snaps_x,
            snaps_y,
            // Chromium snaps a `proximity` container when the snap position is within a third
            // of the scrollport size of where the scroll would otherwise end.
            proximity_range: (snap_type.strictness == ScrollSnapStrictness::Proximity)
                .then(|| size / 3.),
            scroll_padding: self.scroll_padding(size),
            size,
            max_position: LayoutVector2D::new(
                (content_size.width - size.width).max(0.),
                (content_size.height - size.height).max(0.),
            ),
            scrolling_area_origin,
            device_pixel_ratio: self.node().owner_window().device_pixel_ratio().get(),
        })
    }

    fn snap_candidates(&self, context: &SnapContext) -> Vec<SnapCandidate> {
        self.node()
            .traverse_preorder(ShadowIncluding::Yes)
            .skip(1)
            .filter_map(|node| self.snap_candidate(context, node.downcast::<Element>()?))
            .collect()
    }

    /// The snap positions of `element` in this box, or `None` if it is not one of its snap
    /// areas.
    fn snap_candidate(&self, context: &SnapContext, element: &Element) -> Option<SnapCandidate> {
        let style = element.style_from_last_restyle()?;
        let style_box = style.get_box();
        let align = style_box.scroll_snap_align;
        if align.block == ScrollSnapAlignKeyword::None &&
            align.inline == ScrollSnapAlignKeyword::None
        {
            return None;
        }
        // Only the nearest scroll container ancestor of a snap area snaps to it.
        if !element
            .scrolling_box(ScrollContainerQueryFlags::empty())
            .is_some_and(|scrolling_box| *scrolling_box.node() == *self.node())
        {
            return None;
        }
        let area = element.scroll_snap_area()?;

        // Snap areas come from boxes positioned at the current, possibly fractional, scroll
        // position and rounded to app units, so their snap positions carry rounding error.
        // Scroll positions are aligned to device pixels, as when scrolling into view.
        let to_scroll_position = |position: f32, max: f32| {
            ((position * context.device_pixel_ratio).round() / context.device_pixel_ratio)
                .clamp(0., max)
        };
        let origin = context.scrolling_area_origin;
        let x = context
            .snaps_x
            .then(|| {
                Self::snap_position_one_axis(
                    align.inline,
                    area.min_x().to_f32_px() - origin.x,
                    area.max_x().to_f32_px() - origin.x,
                    context.scroll_padding.left,
                    context.size.width - context.scroll_padding.right,
                )
            })
            .flatten()
            .map(|position| to_scroll_position(position, context.max_position.x));
        let y = context
            .snaps_y
            .then(|| {
                Self::snap_position_one_axis(
                    align.block,
                    area.min_y().to_f32_px() - origin.y,
                    area.max_y().to_f32_px() - origin.y,
                    context.scroll_padding.top,
                    context.size.height - context.scroll_padding.bottom,
                )
            })
            .flatten()
            .map(|position| to_scroll_position(position, context.max_position.y));
        Some(SnapCandidate {
            element: DomRoot::from_ref(element),
            x,
            y,
            stop_always: style_box.scroll_snap_stop == ScrollSnapStop::Always,
        })
    }

    /// The scroll position that aligns a snap area spanning `area_start..area_end` with the
    /// snapport spanning `snapport_start..snapport_end`, both in scrolling area coordinates
    /// relative to the scroll position, as `scroll-snap-align` asks.
    fn snap_position_one_axis(
        align: ScrollSnapAlignKeyword,
        area_start: f32,
        area_end: f32,
        snapport_start: f32,
        snapport_end: f32,
    ) -> Option<f32> {
        match align {
            ScrollSnapAlignKeyword::None => None,
            ScrollSnapAlignKeyword::Start => Some(area_start - snapport_start),
            ScrollSnapAlignKeyword::End => Some(area_end - snapport_end),
            ScrollSnapAlignKeyword::Center => {
                Some((area_start + area_end - snapport_start - snapport_end) / 2.)
            },
        }
    }

    /// The index and snap position of the candidate a scroll along one axis ends at, or `None`
    /// if it ends at `intended` without snapping.
    fn choose_snap_target(
        candidates: &[SnapCandidate],
        axis: impl Fn(&SnapCandidate) -> Option<f32>,
        intended: f32,
        origin: Option<f32>,
        proximity_range: Option<f32>,
    ) -> Option<(usize, f32)> {
        let positions = || {
            candidates
                .iter()
                .enumerate()
                .filter_map(|(index, candidate)| Some((index, axis(candidate)?)))
        };
        let closest_to = |target: f32, candidates: &mut dyn Iterator<Item = (usize, f32)>| {
            candidates.min_by(|a, b| (a.1 - target).abs().total_cmp(&(b.1 - target).abs()))
        };
        // Sub-pixel differences between the origin and a snap position are rounding, not a
        // snap position to move past.
        const EPSILON: f32 = 0.5;
        let chosen = match origin.filter(|origin| (intended - origin).abs() > EPSILON) {
            Some(origin) => {
                let direction = (intended - origin).signum();
                let ahead = |position: f32| (position - origin) * direction > EPSILON;
                let chosen = closest_to(intended, &mut positions().filter(|(_, p)| ahead(*p)))
                    .or_else(|| closest_to(intended, &mut positions()))?;
                // A directional scroll must not pass over a snap area that has
                // `scroll-snap-stop: always`.
                // <https://drafts.csswg.org/css-scroll-snap-1/#scroll-snap-stop>
                closest_to(
                    origin,
                    &mut positions().filter(|(index, position)| {
                        candidates[*index].stop_always &&
                            ahead(*position) &&
                            (chosen.1 - position) * direction > EPSILON
                    }),
                )
                .unwrap_or(chosen)
            },
            None => closest_to(intended, &mut positions())?,
        };
        // <https://drafts.csswg.org/css-scroll-snap-1/#valdef-scroll-snap-type-proximity>
        if proximity_range.is_some_and(|range| (chosen.1 - intended).abs() > range) {
            return None;
        }
        Some(chosen)
    }

    /// Step 10 from <https://drafts.csswg.org/cssom-view/#determine-the-scroll-into-view-position>:
    // TODO: we are not considering the coordinate system of the element while deciding the scroll position.
    fn calculate_scroll_position_one_axis(
        state: ScrollAxisState,
        element_start: f32,
        element_end: f32,
        scrollport_start: f32,
        scrollport_end: f32,
        current_scroll_offset: f32,
    ) -> f32 {
        if !state.requirement.compute_need_scroll(
            element_start,
            element_end,
            scrollport_start,
            scrollport_end,
        ) {
            return current_scroll_offset;
        }

        let element_size = element_end - element_start;
        let container_size = scrollport_end - scrollport_start;

        current_scroll_offset +
            match state.position {
                // Step 1 & 5: If inline is "start", then align element start edge with scrolling box start edge.
                ScrollLogicalPosition::Start => element_start - scrollport_start,
                // Step 2 & 6: If inline is "end", then align element end edge with
                // scrolling box end edge.
                ScrollLogicalPosition::End => element_end - scrollport_end,
                // Step 3 & 7: If inline is "center", then align the center of target bounding
                // border box with the center of scrolling box in scrolling box’s inline base direction.
                ScrollLogicalPosition::Center => {
                    (element_start + element_end - scrollport_start - scrollport_end) / 2.0
                },
                // Step 4 & 8: If inline is "nearest",
                ScrollLogicalPosition::Nearest => {
                    // Step 4.2 & 8.2: If element start edge is outside scrolling box start edge and element
                    // size is less than scrolling box size or If element end edge is outside
                    // scrolling box end edge and element size is greater than scrolling box size:
                    // Align element start edge with scrolling box start edge.
                    if (element_start < scrollport_start && element_size <= container_size) ||
                        (element_end > scrollport_end && element_size >= container_size)
                    {
                        element_start - scrollport_start
                    }
                    // Step 4.3 & 8.3: If element end edge is outside scrolling box start edge and element
                    // size is greater than scrolling box size or If element start edge is outside
                    // scrolling box end edge and element size is less than scrolling box size:
                    // Align element end edge with scrolling box end edge.
                    else if (element_end > scrollport_end && element_size < container_size) ||
                        (element_start < scrollport_start && element_size > container_size)
                    {
                        element_end - scrollport_end
                    }
                    // Step 4.1 & 8.1: If element start edge and element end edge are both outside scrolling
                    // box start edge and scrolling box end edge or an invalid situation: Do nothing.
                    else {
                        0.
                    }
                },
            }
    }
}
