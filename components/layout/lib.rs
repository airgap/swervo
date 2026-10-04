/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

//! Layout. Performs layout on the DOM, builds display lists and sends them to be
//! painted.

mod accessibility_tree;
mod cell;
mod context;
mod display_list;
mod dom;
mod dom_traversal;
mod flexbox;
pub mod flow;
mod formatting_contexts;
mod fragment_tree;
pub mod geom;
mod layout_box_base;
mod layout_impl;
mod taffy;
#[macro_use]
mod construct_modern;
mod layout_root;
mod lists;
mod positioned;
mod query;
mod quotes;
mod replaced;
mod sizing;
mod style_ext;
pub mod table;
mod traversal;

use app_units::Au;
pub use cell::ArcRefCell;
pub(crate) use flow::BoxTree;
pub(crate) use fragment_tree::FragmentTree;
pub use layout_impl::LayoutFactoryImpl;
use malloc_size_of_derive::MallocSizeOf;
use servo_arc::Arc as ServoArc;
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;

use crate::formatting_contexts::IndependentFormattingContext;
use crate::geom::LogicalVec2;
use crate::sizing::SizeConstraint;
use crate::style_ext::AspectRatio;

/// At times, a style is "owned" by more than one layout object. For example, text
/// fragments need a handle on their parent inline box's style. In order to make
/// incremental layout easier to implement, another layer of shared ownership is added via
/// [`SharedStyle`]. This allows updating the style in originating layout object and
/// having all "depdendent" objects update automatically.
///
///  Note that this is not a cost-free data structure, so should only be
/// used when necessary.
pub(crate) type SharedStyle = ArcRefCell<ServoArc<ComputedValues>>;

/// Represents the set of constraints that we use when computing the min-content
/// and max-content inline sizes of an element.
pub(crate) struct ConstraintSpace<'a> {
    pub block_size: SizeConstraint,
    pub style: &'a ComputedValues,
    pub preferred_aspect_ratio: Option<AspectRatio>,
    /// See [`IndefiniteContainingBlock::replaced_percentage_block_size`].
    pub replaced_percentage_block_size: Option<Au>,
}

impl<'a> ConstraintSpace<'a> {
    fn new(
        block_size: SizeConstraint,
        style: &'a ComputedValues,
        preferred_aspect_ratio: Option<AspectRatio>,
    ) -> Self {
        Self {
            block_size,
            style,
            preferred_aspect_ratio,
            replaced_percentage_block_size: None,
        }
    }
}

/// A variant of [`ContainingBlock`] that allows an indefinite inline size.
/// Useful for code that is shared for both layout (where we know the inline size
/// of the containing block) and intrinsic sizing (where we don't know it).
pub(crate) struct IndefiniteContainingBlock<'a> {
    pub size: LogicalVec2<Option<Au>>,
    pub style: &'a ComputedValues,
    /// What percentage block sizes of replaced children resolve against instead of
    /// `size.block`, see [`TableCellChildConstraints::replaced_percentage_block_size`].
    pub replaced_percentage_block_size: Option<Au>,
}

impl<'a> IndefiniteContainingBlock<'a> {
    /// The containing block that the sizing properties of a child resolve against.
    fn for_child_sizing(&self, child_is_replaced: bool) -> Self {
        let block = match self.replaced_percentage_block_size {
            Some(block_size) if child_is_replaced => Some(block_size),
            _ => self.size.block,
        };
        Self {
            size: LogicalVec2 {
                inline: self.size.inline,
                block,
            },
            style: self.style,
            replaced_percentage_block_size: self.replaced_percentage_block_size,
        }
    }
}

impl<'a> From<&ConstraintSpace<'a>> for IndefiniteContainingBlock<'a> {
    fn from(constraint_space: &ConstraintSpace<'a>) -> Self {
        Self {
            size: LogicalVec2 {
                inline: None,
                block: constraint_space.block_size.to_definite(),
            },
            style: constraint_space.style,
            replaced_percentage_block_size: constraint_space.replaced_percentage_block_size,
        }
    }
}

impl<'a> From<&'_ ContainingBlock<'a>> for IndefiniteContainingBlock<'a> {
    fn from(containing_block: &ContainingBlock<'a>) -> Self {
        Self {
            size: LogicalVec2 {
                inline: Some(containing_block.size.inline),
                block: containing_block.size.block.to_definite(),
            },
            style: containing_block.style,
            replaced_percentage_block_size: containing_block
                .size
                .table_cell
                .and_then(|table_cell| table_cell.replaced_percentage_block_size),
        }
    }
}

impl<'a> From<&'_ DefiniteContainingBlock<'a>> for IndefiniteContainingBlock<'a> {
    fn from(containing_block: &DefiniteContainingBlock<'a>) -> Self {
        Self {
            size: containing_block.size.map(|v| Some(*v)),
            style: containing_block.style,
            replaced_percentage_block_size: None,
        }
    }
}

#[derive(Clone, Debug, MallocSizeOf, PartialEq)]
pub(crate) struct ContainingBlockSize {
    inline: Au,
    block: SizeConstraint,
    /// How a table cell constrains the block sizes of its children, `None` for other boxes.
    table_cell: Option<TableCellChildConstraints>,
}

/// How a table cell constrains the block sizes of its children, as in Blink.
#[derive(Clone, Copy, Debug, MallocSizeOf, PartialEq)]
pub(crate) struct TableCellChildConstraints {
    /// What percentage block sizes of replaced children resolve against instead of the block
    /// size of the cell: its fixed block size, or `None` to treat them as indefinite. Blink
    /// calls this the replaced percentage resolution block size, and uses it both while the
    /// rows are measured and afterwards.
    replaced_percentage_block_size: Option<Au>,
    /// Whether the cell has a fixed block size or the table a non-`auto` one, which Blink calls
    /// a restricted block size table cell.
    is_restricted: bool,
}

pub(crate) struct ContainingBlock<'a> {
    size: ContainingBlockSize,
    style: &'a ComputedValues,
}

struct DefiniteContainingBlock<'a> {
    size: LogicalVec2<Au>,
    style: &'a ServoArc<ComputedValues>,
}

impl<'a> ContainingBlock<'a> {
    /// The containing block that the sizing properties of a child resolve against.
    fn for_child_sizing(&self, child_is_replaced: bool) -> IndefiniteContainingBlock<'a> {
        let mut containing_block = IndefiniteContainingBlock::from(self);
        if let Some(table_cell) = self.size.table_cell.filter(|_| child_is_replaced) {
            containing_block.size.block = table_cell.replaced_percentage_block_size;
        }
        containing_block
    }

    /// The containing block that the sizing properties of an in-flow block-level child resolve
    /// against.
    fn for_in_flow_block_level_child_sizing(
        &self,
        child: &IndependentFormattingContext,
    ) -> IndefiniteContainingBlock<'a> {
        let mut containing_block = self.for_child_sizing(child.is_replaced());
        // While the rows are measured, Blink sizes a scroll container with a percentage block
        // size that is a child of a restricted cell as if the percentage resolved against zero,
        // so that its overflow doesn't make the row taller than the cell wants to be.
        // <https://drafts.csswg.org/css-tables-3/#row-layout> describes a similar rule.
        let is_measuring_restricted_cell = !self.size.block.is_definite() &&
            self.size
                .table_cell
                .is_some_and(|table_cell| table_cell.is_restricted);
        if is_measuring_restricted_cell &&
            child.is_block_axis_scroll_container_with_percentage_size()
        {
            containing_block.size.block = Some(Au(0));
        }
        containing_block
    }
}

impl<'a> From<&'_ DefiniteContainingBlock<'a>> for ContainingBlock<'a> {
    fn from(definite: &DefiniteContainingBlock<'a>) -> Self {
        ContainingBlock {
            size: ContainingBlockSize {
                inline: definite.size.inline,
                block: SizeConstraint::Definite(definite.size.block),
                table_cell: None,
            },
            style: definite.style,
        }
    }
}

/// Data that is propagated from ancestors to descendants during [`crate::flow::BoxTree`]
/// construction.  This allows data to flow in the reverse direction of the typical layout
/// propoagation, but only during `BoxTree` construction.
#[derive(Clone, Copy, Debug, MallocSizeOf)]
struct PropagatedBoxTreeData {
    allow_percentage_column_in_tables: bool,
}

impl Default for PropagatedBoxTreeData {
    fn default() -> Self {
        Self {
            allow_percentage_column_in_tables: true,
        }
    }
}

impl PropagatedBoxTreeData {
    fn disallowing_percentage_table_columns(&self) -> PropagatedBoxTreeData {
        Self {
            allow_percentage_column_in_tables: false,
        }
    }
}
