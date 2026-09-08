/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Defines data structures which are consumed by `Paint`.

use std::cell::Cell;
use std::collections::HashMap;

use bitflags::bitflags;
use embedder_traits::ViewportDetails;
use euclid::SideOffsets2D;
use malloc_size_of_derive::MallocSizeOf;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_base::Epoch;
use servo_base::id::ScrollTreeNodeId;
use servo_base::print_tree::PrintTree;
use servo_geometry::FastLayoutTransform;
use style::values::specified::Overflow;
use webrender_api::units::{
    LayoutPixel, LayoutPoint, LayoutRect, LayoutSize, LayoutTransform, LayoutVector2D,
};
use webrender_api::{
    ColorF, ExternalScrollId, PipelineId, PropertyBindingKey, ReferenceFrameKind, ScrollLocation,
    SpatialId, StickyOffsetBounds, TransformStyle,
};

/// Matches WebRender's `is_2d_scale_translation` (webrender/src/util.rs:539), whose trait is
/// crate-internal and cannot be imported. Returns true iff the matrix is a pure 2D scale plus
/// translation (no rotation, skew, or z coupling). A `rotateZ(theta)` matrix is `is_2d()` yet
/// NOT scale/translation for theta != 0/180 -- exactly what excludes rotating video tiles from
/// compositor-surface promotion. `NEARLY_ZERO` is kept identical to WebRender's value.
const PROMOTE_NEARLY_ZERO: f32 = 1.0 / 4096.0;
pub fn is_2d_scale_translation(t: &LayoutTransform) -> bool {
    let z = PROMOTE_NEARLY_ZERO;
    (t.m33 - 1.0).abs() < z &&
        (t.m44 - 1.0).abs() < z &&
        t.m12.abs() < z &&
        t.m13.abs() < z &&
        t.m14.abs() < z &&
        t.m21.abs() < z &&
        t.m23.abs() < z &&
        t.m24.abs() < z &&
        t.m31.abs() < z &&
        t.m32.abs() < z &&
        t.m34.abs() < z &&
        t.m43.abs() < z
}

/// A scroll type, describing whether what kind of action originated this scroll request.
/// This is a bitflag as it is also used to track what kinds of [`ScrollType`]s scroll
/// nodes are sensitive to.
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub struct ScrollType(u8);

bitflags! {
    impl ScrollType: u8 {
        /// This node can be scrolled by input events or an input event originated this
        /// scroll.
        const InputEvents = 1 << 0;
        /// This node can be scrolled by script events or script originated this scroll.
        const Script = 1 << 1;
    }
}

/// Convert [Overflow] to [ScrollType].
impl From<Overflow> for ScrollType {
    fn from(overflow: Overflow) -> Self {
        match overflow {
            Overflow::Hidden => ScrollType::Script,
            Overflow::Scroll | Overflow::Auto => ScrollType::Script | ScrollType::InputEvents,
            Overflow::Visible | Overflow::Clip => ScrollType::empty(),
        }
    }
}

/// The [ScrollType] of particular node in the vertical and horizontal axes.
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub struct AxesScrollSensitivity {
    pub x: ScrollType,
    pub y: ScrollType,
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub enum SpatialTreeNodeInfo {
    ReferenceFrame(ReferenceFrameNodeInfo),
    Scroll(ScrollableNodeInfo),
    Sticky(StickyNodeInfo),
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct StickyNodeInfo {
    pub frame_rect: LayoutRect,
    pub margins: SideOffsets2D<Option<f32>, LayoutPixel>,
    pub vertical_offset_bounds: StickyOffsetBounds,
    pub horizontal_offset_bounds: StickyOffsetBounds,
}

impl StickyNodeInfo {
    /// Calculate the sticky offset for this [`StickyNodeInfo`] given information about
    /// sticky positioning from its ancestors.
    ///
    /// This is originally taken from WebRender `SpatialTree` implementation.
    fn calculate_sticky_offset(
        &self,
        viewport_scroll_offset: &LayoutVector2D,
        viewport_rect: &LayoutRect,
    ) -> LayoutVector2D {
        if self.margins.top.is_none() &&
            self.margins.bottom.is_none() &&
            self.margins.left.is_none() &&
            self.margins.right.is_none()
        {
            return LayoutVector2D::zero();
        }

        // The viewport and margins of the item establishes the maximum amount that it can
        // be offset in order to keep it on screen. Since we care about the relationship
        // between the scrolled content and unscrolled viewport we adjust the viewport's
        // position by the scroll offset in order to work with their relative positions on the
        // page.
        let mut sticky_rect = self.frame_rect.translate(*viewport_scroll_offset);

        let mut sticky_offset = LayoutVector2D::zero();
        if let Some(margin) = self.margins.top {
            let top_viewport_edge = viewport_rect.min.y + margin;
            if sticky_rect.min.y < top_viewport_edge {
                // If the sticky rect is positioned above the top edge of the viewport (plus margin)
                // we move it down so that it is fully inside the viewport.
                sticky_offset.y = top_viewport_edge - sticky_rect.min.y;
            }
        }

        // If we don't have a sticky-top offset (sticky_offset.y == 0) then we check for
        // handling the bottom margin case. Note that the "don't have a sticky-top offset"
        // case includes the case where we *had* a sticky-top offset but we reduced it to
        // zero in the above block.
        if sticky_offset.y <= 0.0 &&
            let Some(margin) = self.margins.bottom
        {
            // If sticky_offset.y is nonzero that means we must have set it
            // in the sticky-top handling code above, so this item must have
            // both top and bottom sticky margins. We adjust the item's rect
            // by the top-sticky offset, and then combine any offset from
            // the bottom-sticky calculation into sticky_offset below.
            sticky_rect.min.y += sticky_offset.y;
            sticky_rect.max.y += sticky_offset.y;

            // Same as the above case, but inverted for bottom-sticky items. Here
            // we adjust items upwards, resulting in a negative sticky_offset.y,
            // or reduce the already-present upward adjustment, resulting in a positive
            // sticky_offset.y.
            let bottom_viewport_edge = viewport_rect.max.y - margin;
            if sticky_rect.max.y > bottom_viewport_edge {
                sticky_offset.y += bottom_viewport_edge - sticky_rect.max.y;
            }
        }

        // Same as above, but for the x-axis.
        if let Some(margin) = self.margins.left {
            let left_viewport_edge = viewport_rect.min.x + margin;
            if sticky_rect.min.x < left_viewport_edge {
                sticky_offset.x = left_viewport_edge - sticky_rect.min.x;
            }
        }

        if sticky_offset.x <= 0.0 &&
            let Some(margin) = self.margins.right
        {
            sticky_rect.min.x += sticky_offset.x;
            sticky_rect.max.x += sticky_offset.x;
            let right_viewport_edge = viewport_rect.max.x - margin;
            if sticky_rect.max.x > right_viewport_edge {
                sticky_offset.x += right_viewport_edge - sticky_rect.max.x;
            }
        }

        // The total "sticky offset" and the extra amount we computed as a result of
        // scrolling, stored in sticky_offset needs to be clamped to the provided bounds.
        let clamp =
            |value: f32, bounds: &StickyOffsetBounds| (value).max(bounds.min).min(bounds.max);
        sticky_offset.y = clamp(sticky_offset.y, &self.vertical_offset_bounds);
        sticky_offset.x = clamp(sticky_offset.x, &self.horizontal_offset_bounds);

        sticky_offset
    }
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct ReferenceFrameNodeInfo {
    pub origin: LayoutPoint,
    /// Origin of this frame relative to the document for bounding box queries.
    pub frame_origin_for_query: LayoutPoint,
    pub transform_style: TransformStyle,
    pub transform: FastLayoutTransform,
    pub kind: ReferenceFrameKind,
    /// Set when this frame's transform is being animated on the paint side, so the
    /// reference frame is pushed as a binding rather than a baked value. The matching
    /// [`PaintAnimation`] travels in [`PaintDisplayListInfo::paint_animations`].
    pub animated_transform: Option<PropertyBindingKey<LayoutTransform>>,
}

/// Data stored for nodes in the [ScrollTree] that actually scroll,
/// as opposed to reference frames and sticky nodes which do not.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct ScrollableNodeInfo {
    /// The external scroll id of this node, used to track
    /// it between successive re-layouts.
    pub external_id: ExternalScrollId,

    /// The content rectangle for this scroll node;
    pub content_rect: LayoutRect,

    /// The clip rectange for this scroll node.
    pub clip_rect: LayoutRect,

    /// Whether this `ScrollableNode` is sensitive to input events.
    pub scroll_sensitivity: AxesScrollSensitivity,

    /// The current offset of this scroll node.
    pub offset: LayoutVector2D,

    /// Whether or not the scroll offset of this node has changed and it needs it's
    /// cached transformations invalidated.
    pub offset_changed: Cell<bool>,
}

impl ScrollableNodeInfo {
    fn scroll_to_offset(
        &mut self,
        new_offset: LayoutVector2D,
        context: ScrollType,
    ) -> Option<LayoutVector2D> {
        if !self.scroll_sensitivity.x.contains(context) &&
            !self.scroll_sensitivity.y.contains(context)
        {
            return None;
        }

        let scrollable_size = self.scrollable_size();
        let original_layer_scroll_offset = self.offset;

        if scrollable_size.width > 0. && self.scroll_sensitivity.x.contains(context) {
            self.offset.x = new_offset.x.clamp(0.0, scrollable_size.width);
        }

        if scrollable_size.height > 0. && self.scroll_sensitivity.y.contains(context) {
            self.offset.y = new_offset.y.clamp(0.0, scrollable_size.height);
        }

        if self.offset != original_layer_scroll_offset {
            self.offset_changed.set(true);
            Some(self.offset)
        } else {
            None
        }
    }

    fn scroll_to_webrender_location(
        &mut self,
        scroll_location: ScrollLocation,
        context: ScrollType,
    ) -> Option<LayoutVector2D> {
        if !self.scroll_sensitivity.x.contains(context) &&
            !self.scroll_sensitivity.y.contains(context)
        {
            return None;
        }

        let delta = match scroll_location {
            ScrollLocation::Delta(delta) => delta,
            ScrollLocation::Start => {
                if self.offset.y.round() <= 0.0 {
                    // Nothing to do on this layer.
                    return None;
                }

                self.offset.y = 0.0;
                self.offset_changed.set(true);
                return Some(self.offset);
            },
            ScrollLocation::End => {
                let end_pos = self.scrollable_size().height;
                if self.offset.y.round() >= end_pos {
                    // Nothing to do on this layer.
                    return None;
                }

                self.offset.y = end_pos;
                self.offset_changed.set(true);
                return Some(self.offset);
            },
        };

        self.scroll_to_offset(self.offset + delta, context)
    }
}

impl ScrollableNodeInfo {
    fn scrollable_size(&self) -> LayoutSize {
        self.content_rect.size() - self.clip_rect.size()
    }
}

/// A cached of transforms of a particular [`ScrollTree`] node in both directions:
/// mapping from node-relative points to root-relative points and vice-versa.
///
/// Potential ideas for improvement:
///  - Test optimizing simple translations to avoid having to do full matrix
///    multiplication when transforms are not involved.
#[derive(Clone, Copy, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct ScrollTreeNodeTransformationCache {
    node_to_root_transform: FastLayoutTransform,
    root_to_node_transform: Option<FastLayoutTransform>,
    nearest_scrolling_ancestor_offset: LayoutVector2D,
    nearest_scrolling_ancestor_viewport: LayoutRect,
    cumulative_sticky_offsets: LayoutVector2D,
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
/// A node in a tree of scroll nodes. This may either be a scrollable
/// node which responds to scroll events or a non-scrollable one.
pub struct ScrollTreeNode {
    /// The index of the parent of this node in the tree. If this is
    /// None then this is the root node.
    pub parent: Option<ScrollTreeNodeId>,

    /// The children of this [`ScrollTreeNode`].
    pub children: Vec<ScrollTreeNodeId>,

    /// The WebRender id, which is filled in when this tree is serialiezd
    /// into a WebRender display list.
    pub webrender_id: Option<SpatialId>,

    /// Specific information about this node, depending on whether it is a scroll node
    /// or a reference frame.
    pub info: SpatialTreeNodeInfo,

    /// Cached transformation information that's used to do things like hit testing
    /// and viewport bounding box calculation.
    transformation_cache: Cell<Option<ScrollTreeNodeTransformationCache>>,
}

impl ScrollTreeNode {
    /// Get the WebRender [`SpatialId`] for the given [`ScrollNodeId`]. This will
    /// panic if [`ScrollTree::build_display_list`] has not been called yet.
    pub fn webrender_id(&self) -> SpatialId {
        self.webrender_id
            .expect("Should have called ScrollTree::build_display_list before querying SpatialId")
    }

    /// Get the external id of this node.
    pub fn external_id(&self) -> Option<ExternalScrollId> {
        match self.info {
            SpatialTreeNodeInfo::Scroll(ref info) => Some(info.external_id),
            _ => None,
        }
    }

    /// Get the offset id of this node if it applies.
    pub fn offset(&self) -> Option<LayoutVector2D> {
        match self.info {
            SpatialTreeNodeInfo::Scroll(ref info) => Some(info.offset),
            _ => None,
        }
    }

    /// Scroll this node given a WebRender ScrollLocation. Returns a tuple that can
    /// be used to scroll an individual WebRender scroll frame if the operation
    /// actually changed an offset.
    fn scroll(
        &mut self,
        scroll_location: ScrollLocation,
        context: ScrollType,
    ) -> Option<(ExternalScrollId, LayoutVector2D)> {
        let SpatialTreeNodeInfo::Scroll(ref mut info) = self.info else {
            return None;
        };

        info.scroll_to_webrender_location(scroll_location, context)
            .map(|location| (info.external_id, location))
    }

    pub fn debug_print(&self, print_tree: &mut PrintTree, node_index: usize) {
        match &self.info {
            SpatialTreeNodeInfo::ReferenceFrame(info) => {
                print_tree.new_level(format!(
                    "Reference Frame({node_index}): webrender_id={:?}\
                        \norigin: {:?}\
                        \ntransform_style: {:?}\
                        \ntransform: {:?}\
                        \nkind: {:?}",
                    self.webrender_id, info.origin, info.transform_style, info.transform, info.kind,
                ));
            },
            SpatialTreeNodeInfo::Scroll(info) => {
                print_tree.new_level(format!(
                    "Scroll Frame({node_index}): webrender_id={:?}\
                        \nexternal_id: {:?}\
                        \ncontent_rect: {:?}\
                        \nclip_rect: {:?}\
                        \nscroll_sensitivity: {:?}\
                        \noffset: {:?}",
                    self.webrender_id,
                    info.external_id,
                    info.content_rect,
                    info.clip_rect,
                    info.scroll_sensitivity,
                    info.offset,
                ));
            },
            SpatialTreeNodeInfo::Sticky(info) => {
                print_tree.new_level(format!(
                    "Sticky Frame({node_index}): webrender_id={:?}\
                        \nframe_rect: {:?}\
                        \nmargins: {:?}\
                        \nhorizontal_offset_bounds: {:?}\
                        \nvertical_offset_bounds: {:?}",
                    self.webrender_id,
                    info.frame_rect,
                    info.margins,
                    info.horizontal_offset_bounds,
                    info.vertical_offset_bounds,
                ));
            },
        };
    }

    fn invalidate_cached_transforms(&self, scroll_tree: &ScrollTree, ancestors_invalid: bool) {
        let node_invalid = match &self.info {
            SpatialTreeNodeInfo::Scroll(info) => info.offset_changed.take(),
            _ => false,
        };

        let invalid = node_invalid || ancestors_invalid;
        if invalid {
            self.transformation_cache.set(None);
        }

        for child_id in &self.children {
            scroll_tree
                .get_node(*child_id)
                .invalidate_cached_transforms(scroll_tree, invalid);
        }
    }
}

/// A tree of spatial nodes, which mirrors the spatial nodes in the WebRender
/// display list, except these are used for scrolling in `Paint` so that
/// new offsets can be sent to WebRender.
#[derive(Clone, Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct ScrollTree {
    /// A list of `Paint`-side scroll nodes that describe the tree
    /// of WebRender spatial nodes, used by `Paint` to scroll the
    /// contents of the display list.
    pub nodes: Vec<ScrollTreeNode>,
}

impl ScrollTree {
    /// Add a scroll node to this ScrollTree returning the id of the new node.
    pub fn add_scroll_tree_node(
        &mut self,
        parent: Option<ScrollTreeNodeId>,
        info: SpatialTreeNodeInfo,
    ) -> ScrollTreeNodeId {
        self.nodes.push(ScrollTreeNode {
            parent,
            children: Vec::new(),
            webrender_id: None,
            info,
            transformation_cache: Cell::default(),
        });

        let new_node_id = ScrollTreeNodeId {
            index: self.nodes.len() - 1,
        };

        if let Some(parent_id) = parent {
            self.get_node_mut(parent_id).children.push(new_node_id);
        }

        new_node_id
    }

    /// Once WebRender display list construction is complete for this [`ScrollTree`], update
    /// the mapping of nodes to WebRender [`SpatialId`]s.
    pub fn update_mapping(&mut self, mapping: Vec<SpatialId>) {
        for (spatial_id, node) in mapping.into_iter().zip(self.nodes.iter_mut()) {
            node.webrender_id = Some(spatial_id);
        }
    }

    /// Get a mutable reference to the node with the given index.
    pub fn get_node_mut(&mut self, id: ScrollTreeNodeId) -> &mut ScrollTreeNode {
        &mut self.nodes[id.index]
    }

    /// Get an immutable reference to the node with the given index.
    pub fn get_node(&self, id: ScrollTreeNodeId) -> &ScrollTreeNode {
        &self.nodes[id.index]
    }

    /// Get the WebRender [`SpatialId`] for the given [`ScrollNodeId`]. This will
    /// panic if [`ScrollTree::build_display_list`] has not been called yet.
    pub fn webrender_id(&self, id: ScrollTreeNodeId) -> SpatialId {
        self.get_node(id).webrender_id()
    }

    pub fn scroll_node_or_ancestor_inner(
        &mut self,
        scroll_node_id: ScrollTreeNodeId,
        scroll_location: ScrollLocation,
        context: ScrollType,
    ) -> Option<(ExternalScrollId, LayoutVector2D)> {
        let parent = {
            let node = &mut self.get_node_mut(scroll_node_id);
            let result = node.scroll(scroll_location, context);
            if result.is_some() {
                return result;
            }
            node.parent
        };

        parent
            .and_then(|parent| self.scroll_node_or_ancestor_inner(parent, scroll_location, context))
    }

    fn node_with_external_scroll_node_id(
        &self,
        external_id: ExternalScrollId,
    ) -> Option<ScrollTreeNodeId> {
        self.nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match &node.info {
                SpatialTreeNodeInfo::Scroll(info) if info.external_id == external_id => {
                    Some(ScrollTreeNodeId { index })
                },
                _ => None,
            })
    }

    /// Scroll the scroll node with the given [`ExternalScrollId`] on this scroll tree. If
    /// the node cannot be scrolled, because it's already scrolled to the maximum scroll
    /// extent, try to scroll an ancestor of this node. Returns the node scrolled and the
    /// new offset if a scroll was performed, otherwise returns None.
    pub fn scroll_node_or_ancestor(
        &mut self,
        external_id: ExternalScrollId,
        scroll_location: ScrollLocation,
        context: ScrollType,
    ) -> Option<(ExternalScrollId, LayoutVector2D)> {
        let scroll_node_id = self.node_with_external_scroll_node_id(external_id)?;
        let result = self.scroll_node_or_ancestor_inner(scroll_node_id, scroll_location, context);
        if result.is_some() {
            self.invalidate_cached_transforms();
        }
        result
    }

    /// Given an [`ExternalScrollId`] and an offset, update the scroll offset of the scroll node
    /// with the given id.
    pub fn set_scroll_offset_for_node_with_external_scroll_id(
        &mut self,
        external_scroll_id: ExternalScrollId,
        offset: LayoutVector2D,
        context: ScrollType,
    ) -> Option<LayoutVector2D> {
        let result = self.nodes.iter_mut().find_map(|node| match node.info {
            SpatialTreeNodeInfo::Scroll(ref mut scroll_info)
                if scroll_info.external_id == external_scroll_id =>
            {
                scroll_info.scroll_to_offset(offset, context)
            },
            _ => None,
        });

        if result.is_some() {
            self.invalidate_cached_transforms();
        }

        result
    }

    /// Given a set of all scroll offsets coming from the Servo renderer, update all of the offsets
    /// for nodes that actually exist in this tree.
    pub fn set_all_scroll_offsets(
        &mut self,
        offsets: &FxHashMap<ExternalScrollId, LayoutVector2D>,
    ) {
        for node in self.nodes.iter_mut() {
            if let SpatialTreeNodeInfo::Scroll(ref mut scroll_info) = node.info &&
                let Some(offset) = offsets.get(&scroll_info.external_id)
            {
                scroll_info.scroll_to_offset(*offset, ScrollType::Script);
            }
        }

        self.invalidate_cached_transforms();
    }

    /// Set the offsets of all scrolling nodes in this tree to 0.
    pub fn reset_all_scroll_offsets(&mut self) {
        for node in self.nodes.iter_mut() {
            if let SpatialTreeNodeInfo::Scroll(ref mut scroll_info) = node.info {
                scroll_info.scroll_to_offset(LayoutVector2D::zero(), ScrollType::Script);
            }
        }

        self.invalidate_cached_transforms();
    }

    /// Collect all of the scroll offsets of the scrolling nodes of this tree into a
    /// [`HashMap`] which can be applied to another tree.
    pub fn scroll_offsets(&self) -> FxHashMap<ExternalScrollId, LayoutVector2D> {
        HashMap::from_iter(self.nodes.iter().filter_map(|node| match node.info {
            SpatialTreeNodeInfo::Scroll(ref scroll_info) => {
                Some((scroll_info.external_id, scroll_info.offset))
            },
            _ => None,
        }))
    }

    /// Get the scroll offset for the given [`ExternalScrollId`] or `None` if that node cannot
    /// be found in the tree.
    pub fn scroll_offset(&self, id: ExternalScrollId) -> Option<LayoutVector2D> {
        self.nodes.iter().find_map(|node| match node.info {
            SpatialTreeNodeInfo::Scroll(ref info) if info.external_id == id => Some(info.offset),
            _ => None,
        })
    }

    /// Find a transformation that can convert a point in the node coordinate system to a
    /// point in the root coordinate system.
    pub fn cumulative_node_to_root_transform(
        &self,
        node_id: ScrollTreeNodeId,
    ) -> FastLayoutTransform {
        self.cumulative_node_transform(node_id)
            .node_to_root_transform
    }

    /// Find a transformation that can convert a point in the root coordinate system to a
    /// point in the coordinate system of the given node. This may be `None` if the cumulative
    /// transform is uninvertible.
    pub fn cumulative_root_to_node_transform(
        &self,
        node_id: ScrollTreeNodeId,
    ) -> Option<FastLayoutTransform> {
        self.cumulative_node_transform(node_id)
            .root_to_node_transform
    }

    /// Find the untransformed offset in the initial containing block of the nearest
    /// inclusive ancestor reference frame for the given spatial tree node.
    pub fn reference_frame_offset(&self, node_id: ScrollTreeNodeId) -> LayoutPoint {
        let mut maybe_node_id = Some(node_id);
        while let Some(node_id) = maybe_node_id {
            let node = self.get_node(node_id);
            if let SpatialTreeNodeInfo::ReferenceFrame(reference_frame) = &node.info {
                return reference_frame.frame_origin_for_query;
            }
            maybe_node_id = node.parent;
        }
        Default::default()
    }

    /// Find the cumulative offsets of sticky positioned boxes from the given node up to
    /// the root.
    pub fn cumulative_sticky_offsets(&self, node_id: ScrollTreeNodeId) -> LayoutVector2D {
        self.cumulative_node_transform(node_id)
            .cumulative_sticky_offsets
    }

    fn cumulative_node_transform(
        &self,
        node_id: ScrollTreeNodeId,
    ) -> ScrollTreeNodeTransformationCache {
        let node = self.get_node(node_id);
        if let Some(cached_transforms) = node.transformation_cache.get() {
            return cached_transforms;
        }

        let transforms = self.cumulative_node_transform_inner(node);
        node.transformation_cache.set(Some(transforms));
        transforms
    }

    /// Traverse a scroll node to its root to calculate the transform.
    fn cumulative_node_transform_inner(
        &self,
        node: &ScrollTreeNode,
    ) -> ScrollTreeNodeTransformationCache {
        let parent_transforms = node
            .parent
            .map(|parent_id| self.cumulative_node_transform(parent_id))
            .unwrap_or_default();

        let node_to_root_transform = |node_to_parent_transform: FastLayoutTransform| {
            node_to_parent_transform.then(&parent_transforms.node_to_root_transform)
        };
        let root_to_node_transform = |parent_to_node_transform: FastLayoutTransform| {
            parent_transforms
                .root_to_node_transform
                .map_or(parent_to_node_transform, |parent_transform| {
                    parent_transform.then(&parent_to_node_transform)
                })
        };

        match &node.info {
            SpatialTreeNodeInfo::ReferenceFrame(info) => {
                // To apply a transformation we need to make sure the rectangle's
                // coordinate space is the same as reference frame's coordinate space.
                let offset = info.frame_origin_for_query.to_vector();
                let node_to_parent_transform =
                    info.transform.pre_translate(-offset).then_translate(offset);
                let parent_to_node_transform = info.transform.inverse().map(|inverse_transform| {
                    FastLayoutTransform::Offset(-info.origin.to_vector()).then(&inverse_transform)
                });
                ScrollTreeNodeTransformationCache {
                    node_to_root_transform: node_to_root_transform(node_to_parent_transform),
                    root_to_node_transform: parent_to_node_transform.map(root_to_node_transform),
                    nearest_scrolling_ancestor_viewport: parent_transforms
                        .nearest_scrolling_ancestor_viewport
                        .translate(-info.origin.to_vector()),
                    nearest_scrolling_ancestor_offset: parent_transforms
                        .nearest_scrolling_ancestor_offset,
                    cumulative_sticky_offsets: parent_transforms.cumulative_sticky_offsets,
                }
            },
            SpatialTreeNodeInfo::Scroll(info) => {
                let node_to_parent_transform = FastLayoutTransform::Offset(-info.offset);
                let parent_to_node_transform = node_to_parent_transform.inverse();
                ScrollTreeNodeTransformationCache {
                    node_to_root_transform: node_to_root_transform(node_to_parent_transform),
                    root_to_node_transform: parent_to_node_transform.map(root_to_node_transform),
                    nearest_scrolling_ancestor_viewport: info.clip_rect,
                    nearest_scrolling_ancestor_offset: -info.offset,
                    cumulative_sticky_offsets: parent_transforms.cumulative_sticky_offsets,
                }
            },

            SpatialTreeNodeInfo::Sticky(info) => {
                let offset = info.calculate_sticky_offset(
                    &parent_transforms.nearest_scrolling_ancestor_offset,
                    &parent_transforms.nearest_scrolling_ancestor_viewport,
                );
                let node_to_parent_transform = FastLayoutTransform::Offset(offset);
                let parent_to_node_transform = node_to_parent_transform.inverse();
                ScrollTreeNodeTransformationCache {
                    node_to_root_transform: node_to_root_transform(node_to_parent_transform),
                    root_to_node_transform: parent_to_node_transform.map(root_to_node_transform),
                    nearest_scrolling_ancestor_viewport: parent_transforms
                        .nearest_scrolling_ancestor_viewport,
                    nearest_scrolling_ancestor_offset: parent_transforms
                        .nearest_scrolling_ancestor_offset +
                        offset,
                    cumulative_sticky_offsets: parent_transforms.cumulative_sticky_offsets + offset,
                }
            },
        }
    }

    fn invalidate_cached_transforms(&self) {
        let Some(root_node) = self.nodes.first() else {
            return;
        };
        root_node.invalidate_cached_transforms(self, false /* ancestors_invalid */);
    }

    fn external_scroll_id_for_scroll_tree_node(
        &self,
        id: ScrollTreeNodeId,
    ) -> Option<ExternalScrollId> {
        let mut maybe_node = Some(self.get_node(id));

        while let Some(node) = maybe_node {
            if let Some(external_scroll_id) = node.external_id() {
                return Some(external_scroll_id);
            }
            maybe_node = node.parent.map(|id| self.get_node(id));
        }

        None
    }
}

/// In order to pretty print the [ScrollTree] structure, we are converting
/// the node list inside the tree to be a adjacency list. The adjacency list
/// then is used for the [ScrollTree::debug_print_traversal] of the tree.
///
/// This preprocessing helps decouples print logic a lot from its construction.
type AdjacencyListForPrint = Vec<Vec<ScrollTreeNodeId>>;

/// Implementation of [ScrollTree] that is related to debugging.
// FIXME: probably we could have a universal trait for this. Especially for
//        structures that utilizes PrintTree.
impl ScrollTree {
    fn nodes_in_adjacency_list(&self) -> AdjacencyListForPrint {
        let mut adjacency_list: AdjacencyListForPrint = vec![Default::default(); self.nodes.len()];

        for (node_index, node) in self.nodes.iter().enumerate() {
            let current_id = ScrollTreeNodeId { index: node_index };
            if let Some(parent_id) = node.parent {
                adjacency_list[parent_id.index].push(current_id);
            }
        }

        adjacency_list
    }

    fn debug_print_traversal(
        &self,
        print_tree: &mut PrintTree,
        current_id: ScrollTreeNodeId,
        adjacency_list: &[Vec<ScrollTreeNodeId>],
    ) {
        for node_id in &adjacency_list[current_id.index] {
            self.nodes[node_id.index].debug_print(print_tree, node_id.index);
            self.debug_print_traversal(print_tree, *node_id, adjacency_list);
        }
        print_tree.end_level();
    }

    /// Print the [ScrollTree]. Particularly, we are printing the node in
    /// preorder traversal. The order of the nodes will depends of the
    /// index of a node in the [ScrollTree] which corresponds to the
    /// declarations of the nodes.
    // TODO(stevennovaryo): add information about which fragment that
    //                      defines this node.
    pub fn debug_print(&self) {
        let mut print_tree = PrintTree::new("Scroll Tree");

        let adj_list = self.nodes_in_adjacency_list();
        let root_id = ScrollTreeNodeId { index: 0 };

        self.nodes[root_id.index].debug_print(&mut print_tree, root_id.index);
        self.debug_print_traversal(&mut print_tree, root_id, &adj_list);
        print_tree.end_level();
    }
}

/// The shape of one segment of a paint-side animation.
///
/// A CSS timing function, reduced to what the paint thread needs to evaluate it. Layout
/// keeps the authoritative animation; this is only enough to play the next stretch of it.
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, Serialize)]
pub enum PaintAnimationEasing {
    Linear,
    /// The four control-point coordinates of a `cubic-bezier()`.
    CubicBezier(f32, f32, f32, f32),
    /// `steps(count, jump_start)`; `jump_start` distinguishes `start` from `end`.
    Steps(u32, bool),
}

impl PaintAnimationEasing {
    /// Map linear progress through a segment to eased progress.
    ///
    /// The bezier solve is Newton's method over the curve's x, which is what every engine
    /// does here; ten iterations is far more than the four or five it takes to converge to
    /// float precision on the curves CSS allows.
    pub fn ease(&self, progress: f64) -> f64 {
        let progress = progress.clamp(0.0, 1.0);
        match *self {
            Self::Linear => progress,
            Self::CubicBezier(x1, y1, x2, y2) => {
                let (x1, y1, x2, y2) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64);
                let sample = |a: f64, b: f64, t: f64| {
                    let inverse = 1.0 - t;
                    3.0 * inverse * inverse * t * a + 3.0 * inverse * t * t * b + t * t * t
                };
                let mut t = progress;
                for _ in 0..10 {
                    let x = sample(x1, x2, t) - progress;
                    if x.abs() < 1e-6 {
                        break;
                    }
                    let inverse = 1.0 - t;
                    let derivative = 3.0 * inverse * inverse * x1
                        + 6.0 * inverse * t * (x2 - x1)
                        + 3.0 * t * t * (1.0 - x2);
                    if derivative.abs() < 1e-6 {
                        break;
                    }
                    t -= x / derivative;
                    t = t.clamp(0.0, 1.0);
                }
                sample(y1, y2, t)
            },
            Self::Steps(count, jump_start) => {
                let count = count.max(1) as f64;
                let step = (progress * count).floor() + if jump_start { 1.0 } else { 0.0 };
                (step / count).clamp(0.0, 1.0)
            },
        }
    }
}

/// One stretch of a paint-side animation, in seconds from the animation's start.
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct PaintAnimationSegment<T> {
    pub start: f64,
    pub end: f64,
    pub from: T,
    pub to: T,
    pub easing: PaintAnimationEasing,
}

/// The animated property, its WebRender binding, and the segments to play.
///
/// ***Transform segments are interpolated as matrices, so layout subdivides.*** CSS
/// interpolates transform *lists* componentwise, and a matrix lerp only agrees with that
/// for translation and scale. Layout resolves the exact value at each sub-segment
/// boundary, which keeps the error inside a sub-segment invisible without teaching the
/// paint thread anything about transform lists.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub enum PaintAnimationProperty {
    Opacity(PropertyBindingKey<f32>, Vec<PaintAnimationSegment<f32>>),
    Transform(
        PropertyBindingKey<LayoutTransform>,
        Vec<PaintAnimationSegment<LayoutTransform>>,
    ),
}

/// Which property of an element a binding key refers to, so one element can bind more
/// than one property without the keys colliding.
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, PartialEq, Serialize)]
pub enum PaintAnimatedProperty {
    Opacity = 0,
    Transform = 1,
}

/// The WebRender property binding key for one element's animated property.
///
/// ***The top bit is reserved so these can never collide with the caret's key.*** The
/// caret builds its key out of the pipeline id itself (`PropertyBindingKey::new(pipeline)`),
/// so an animation key must never land on a pipeline index. Real pipeline indices do not
/// approach 2^31, so setting the top bit separates the two spaces outright instead of
/// probabilistically.
///
/// The rest is a hash of the node, which is unique process-wide. That also makes the key
/// stable across display lists: an element keeps its binding for as long as its animation
/// runs, so WebRender keeps applying values to the same place.
pub fn paint_animation_binding_key<T>(
    pipeline_id: PipelineId,
    node: u64,
    property: PaintAnimatedProperty,
) -> PropertyBindingKey<T> {
    let mut hash = node;
    hash = hash.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= hash >> 32;
    hash = hash.wrapping_add(property as u64);
    let uid = (hash as u32) | 0x8000_0000;
    PropertyBindingKey::new(((pipeline_id.0 as u64) << 32) | uid as u64)
}

impl PaintAnimationSegment<f32> {
    /// Two sampled values closer than this count as the same value.
    pub const EPSILON: f32 = 1.0 / 2048.0;

    /// Turn evenly spaced samples into as few straight segments as reproduce them.
    ///
    /// ***Sampling and then merging, rather than reading keyframes, is deliberate.*** The
    /// sample is whatever the style system says the value is, so easing, iteration count,
    /// direction and fill are all handled by the code that already implements them, and
    /// this stays a few lines instead of a second implementation of CSS animation. The
    /// merge is what keeps that cheap: a linear fade -- the common case -- collapses to a
    /// single segment no matter how densely it was sampled, and the result travels with
    /// every display list and is cloned once per painter.
    pub fn from_samples(samples: &[f32], step: f64) -> Vec<Self> {
        let mut segments: Vec<Self> = Vec::new();
        let mut start_index = 0usize;
        for index in 1..samples.len() {
            let start = samples[start_index];
            let end = samples[index];
            let span = (index - start_index) as f32;
            // Does every sample in between sit on the line from `start` to `end`?
            let straight = (start_index + 1..index).all(|between| {
                let expected = start + (end - start) * ((between - start_index) as f32 / span);
                (samples[between] - expected).abs() <= Self::EPSILON
            });
            if straight {
                continue;
            }
            segments.push(Self {
                start: start_index as f64 * step,
                end: (index - 1) as f64 * step,
                from: samples[start_index],
                to: samples[index - 1],
                easing: PaintAnimationEasing::Linear,
            });
            start_index = index - 1;
        }
        if start_index + 1 < samples.len() {
            segments.push(Self {
                start: start_index as f64 * step,
                end: (samples.len() - 1) as f64 * step,
                from: samples[start_index],
                to: samples[samples.len() - 1],
                easing: PaintAnimationEasing::Linear,
            });
        }
        // A run that never moves is not an animation. Returning it anyway would let a
        // caller bind a property to a value that never changes, and a bound property
        // WebRender is never given a value for keeps whatever it last held -- which is
        // worse than never binding it.
        if segments
            .iter()
            .all(|segment| (segment.to - segment.from).abs() <= Self::EPSILON)
        {
            return Vec::new();
        }
        segments
    }
}

impl PaintAnimationSegment<LayoutTransform> {
    /// How far apart two matrices may be and still count as the same one.
    ///
    /// A matrix mixes translations in pixels with scales and rotations around one, so a
    /// single tolerance has to be judged against the largest term. This is a hundredth of
    /// a pixel at unit scale, which is below anything a display can show.
    pub const EPSILON: f32 = 0.01;

    /// The same merge as the scalar case, componentwise on the matrix.
    ///
    /// ***Matrices are interpolated componentwise here and CSS interpolates transform
    /// lists.*** Those agree exactly for translation and scale, and diverge for rotation,
    /// which is why layout samples at the rate it does and lets the merge decide: a
    /// rotation simply fails the straightness test and keeps more segments, so the error
    /// inside any surviving segment stays under `EPSILON` by construction.
    pub fn from_samples(samples: &[LayoutTransform], step: f64) -> Vec<Self> {
        let straight_between = |from: &LayoutTransform, to: &LayoutTransform, at: &LayoutTransform, progress: f32| {
            let from = from.to_array();
            let to = to.to_array();
            let at = at.to_array();
            (0..16).all(|index| {
                let expected = from[index] + (to[index] - from[index]) * progress;
                (at[index] - expected).abs() <= Self::EPSILON
            })
        };
        let differs = |a: &LayoutTransform, b: &LayoutTransform| {
            let a = a.to_array();
            let b = b.to_array();
            (0..16).any(|index| (a[index] - b[index]).abs() > Self::EPSILON)
        };

        let mut segments: Vec<Self> = Vec::new();
        let mut start_index = 0usize;
        for index in 1..samples.len() {
            let span = (index - start_index) as f32;
            let straight = (start_index + 1..index).all(|between| {
                straight_between(
                    &samples[start_index],
                    &samples[index],
                    &samples[between],
                    (between - start_index) as f32 / span,
                )
            });
            if straight {
                continue;
            }
            segments.push(Self {
                start: start_index as f64 * step,
                end: (index - 1) as f64 * step,
                from: samples[start_index],
                to: samples[index - 1],
                easing: PaintAnimationEasing::Linear,
            });
            start_index = index - 1;
        }
        if start_index + 1 < samples.len() {
            segments.push(Self {
                start: start_index as f64 * step,
                end: (samples.len() - 1) as f64 * step,
                from: samples[start_index],
                to: samples[samples.len() - 1],
                easing: PaintAnimationEasing::Linear,
            });
        }
        if segments
            .iter()
            .all(|segment| !differs(&segment.from, &segment.to))
        {
            return Vec::new();
        }
        segments
    }
}

/// An animation the paint thread can play without asking script for anything.
///
/// ***This is a prediction, and script rebases it.*** Every display list carries the
/// animations that are live at the moment it was built, sampled forward from that moment.
/// While script keeps up, each new display list replaces the previous prediction with the
/// truth and nothing drifts. When script stalls -- laying out a new configuration,
/// starting fifty video pipelines -- the paint thread keeps playing the last prediction
/// instead of showing a frozen frame. That is the whole point: measured on the 4-GPU wall
/// (log_ani_perf/03), a single content-switch task held script for 9.5 seconds while the
/// painters were idle enough to composite 60 frames a second the entire time.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct PaintAnimation {
    pub property: PaintAnimationProperty,
    /// Seconds from the display list's creation to the animation's zero point. Negative
    /// for an animation already under way, which is the usual case.
    pub offset_from_display_list: f64,
    /// Whether the segments run to the animation's end. When false the animation outlives
    /// what was sampled and the paint thread holds the last value until script catches up.
    pub complete: bool,
}

/// A data structure which stores `Paint`-side information about
/// display lists sent to `Paint`.
#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct PaintDisplayListInfo {
    /// The WebRender [PipelineId] of this display list.
    pub pipeline_id: PipelineId,

    /// The [`ViewportDetails`] that describe the viewport in the script/layout thread at
    /// the time this display list was created.
    pub viewport_details: ViewportDetails,

    /// The size of this display list's content.
    pub content_size: LayoutSize,

    /// The epoch of the display list.
    pub epoch: Epoch,

    /// A ScrollTree used by `Paint` to scroll the contents of the
    /// display list.
    pub scroll_tree: ScrollTree,

    /// The `ScrollTreeNodeId` of the root reference frame of this info's scroll
    /// tree.
    pub root_reference_frame_id: ScrollTreeNodeId,

    /// The `ScrollTreeNodeId` of the topmost scrolling frame of this info's scroll
    /// tree.
    pub root_scroll_node_id: ScrollTreeNodeId,

    /// From <https://www.w3.org/TR/paint-timing/#paintable>:
    /// Whether the display list contains paintable items.
    pub is_paintable: bool,

    /// From <https://www.w3.org/TR/paint-timing/#contentful>:
    /// Contentful paint i.e. whether the display list contains items of type
    /// text, image, non-white canvas or SVG). Used by metrics.
    pub is_contentful: bool,

    /// Whether the first layout or a subsequent (incremental) layout triggered this
    /// display list creation.
    pub first_reflow: bool,

    /// If this display list contains a blinking caret, this value will be filled with its animation
    /// key and original color value so that the painter can animate the caret.
    pub caret_property_binding: Option<(PropertyBindingKey<ColorF>, ColorF)>,

    /// The animations the paint thread should keep playing on its own until the next
    /// display list arrives. See [`PaintAnimation`].
    pub paint_animations: Vec<PaintAnimation>,
}

impl PaintDisplayListInfo {
    /// Create a new PaintDisplayListInfo with the root reference frame
    /// and scroll frame already added to the scroll tree.
    pub fn new(
        viewport_details: ViewportDetails,
        content_size: LayoutSize,
        pipeline_id: PipelineId,
        epoch: Epoch,
        viewport_scroll_sensitivity: AxesScrollSensitivity,
        first_reflow: bool,
    ) -> Self {
        let mut scroll_tree = ScrollTree::default();
        let root_reference_frame_id = scroll_tree.add_scroll_tree_node(
            None,
            SpatialTreeNodeInfo::ReferenceFrame(ReferenceFrameNodeInfo {
                origin: Default::default(),
                frame_origin_for_query: Default::default(),
                transform_style: TransformStyle::Flat,
                transform: FastLayoutTransform::identity(),
                kind: ReferenceFrameKind::default(),
                animated_transform: None,
            }),
        );
        let root_scroll_node_id = scroll_tree.add_scroll_tree_node(
            Some(root_reference_frame_id),
            SpatialTreeNodeInfo::Scroll(ScrollableNodeInfo {
                external_id: ExternalScrollId(0, pipeline_id),
                content_rect: LayoutRect::from_origin_and_size(LayoutPoint::zero(), content_size),
                clip_rect: LayoutRect::from_origin_and_size(
                    LayoutPoint::zero(),
                    viewport_details.layout_size(),
                ),
                scroll_sensitivity: viewport_scroll_sensitivity,
                offset: LayoutVector2D::zero(),
                offset_changed: Cell::new(false),
            }),
        );

        PaintDisplayListInfo {
            pipeline_id,
            viewport_details,
            content_size,
            epoch,
            scroll_tree,
            root_reference_frame_id,
            root_scroll_node_id,
            is_paintable: false,
            is_contentful: false,
            first_reflow,
            caret_property_binding: Default::default(),
            paint_animations: Vec::new(),
        }
    }

    pub fn external_scroll_id_for_scroll_tree_node(
        &self,
        id: ScrollTreeNodeId,
    ) -> ExternalScrollId {
        self.scroll_tree
            .external_scroll_id_for_scroll_tree_node(id)
            .unwrap_or(ExternalScrollId(0, self.pipeline_id))
    }
}

#[cfg(test)]
mod promote_tests {
    use euclid::Angle;
    use webrender_api::units::LayoutTransform;

    use super::is_2d_scale_translation;

    #[test]
    fn identity_and_scale_translate_are_2d() {
        assert!(is_2d_scale_translation(&LayoutTransform::identity()));
        assert!(is_2d_scale_translation(&LayoutTransform::scale(0.78, 0.78, 1.0)));
        assert!(is_2d_scale_translation(&LayoutTransform::translation(30.0, -12.0, 0.0)));
        let m = LayoutTransform::scale(0.9, 0.9, 1.0).then_translate(euclid::vec3(5.0, 5.0, 0.0));
        assert!(is_2d_scale_translation(&m));
    }

    #[test]
    fn rotatez_nonzero_is_not_2d_scale_translation() {
        // rotateZ(45deg): is_2d()엔 true지만 scale/translation은 아님 -> false.
        let m = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(45.0));
        assert!(!is_2d_scale_translation(&m));
        let m90 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(90.0));
        assert!(!is_2d_scale_translation(&m90));
    }

    #[test]
    fn rotatez_0_and_180_degenerate_to_2d() {
        let m0 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(0.0));
        assert!(is_2d_scale_translation(&m0));
        // 180deg = scale(-1,-1): 회전 항 0 -> 2D (플래핑의 승격 순간).
        let m180 = LayoutTransform::rotation(0.0, 0.0, 1.0, Angle::degrees(180.0));
        assert!(is_2d_scale_translation(&m180));
    }

    #[test]
    fn rotatey_is_not_2d_scale_translation() {
        // 3D Y-플립: z 결합 -> false.
        let m = LayoutTransform::rotation(0.0, 1.0, 0.0, Angle::degrees(45.0));
        assert!(!is_2d_scale_translation(&m));
    }
}

#[cfg(test)]
mod paint_animation_tests {
    use super::*;

    fn replay(segments: &[PaintAnimationSegment<f32>], step: f64, count: usize) -> Vec<f32> {
        (0..count)
            .map(|index| {
                let time = index as f64 * step;
                let segment = segments
                    .iter()
                    .find(|segment| time < segment.end)
                    .unwrap_or_else(|| segments.last().expect("no segments"));
                let span = segment.end - segment.start;
                let progress = if span > 0.0 {
                    ((time - segment.start) / span).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                segment.from + (segment.to - segment.from) * progress
            })
            .collect()
    }

    /// ***A straight fade must not cost a segment per sample.*** These segments travel
    /// with every display list and are cloned once per painter, so a representation that
    /// grew with the sample rate would make the fix a cost of its own.
    #[test]
    fn a_linear_run_collapses_to_one_segment() {
        let samples: Vec<f32> = (0..=60).map(|index| index as f32 / 60.0).collect();
        let segments = PaintAnimationSegment::<f32>::from_samples(&samples, 1.0 / 60.0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].from, 0.0);
        assert_eq!(segments[0].to, 1.0);
    }

    /// A curve keeps enough segments to stay within tolerance at every sample it was
    /// built from -- the merge is allowed to be lossy only below what anyone can see.
    #[test]
    fn a_curve_is_reproduced_within_tolerance() {
        let step = 1.0 / 60.0;
        let samples: Vec<f32> = (0..=60)
            .map(|index| {
                let t = index as f32 / 60.0;
                t * t * (3.0 - 2.0 * t)
            })
            .collect();
        let segments = PaintAnimationSegment::<f32>::from_samples(&samples, step);
        assert!(segments.len() > 1, "a smoothstep is not a straight line");
        for (index, replayed) in replay(&segments, step, samples.len()).iter().enumerate() {
            assert!(
                (replayed - samples[index]).abs() <= PaintAnimationSegment::<f32>::EPSILON * 2.0,
                "sample {index}: replayed {replayed}, sampled {}",
                samples[index]
            );
        }
    }

    /// A constant run produces nothing to play, which is what tells the caller not to bind
    /// the property at all. Binding with no values behind it would be worse than not
    /// binding: WebRender would hold whatever it last had.
    #[test]
    fn a_constant_run_produces_nothing_to_play() {
        let samples = vec![0.5f32; 61];
        assert!(PaintAnimationSegment::<f32>::from_samples(&samples, 1.0 / 60.0).is_empty());
    }

    /// ***Animation keys must never collide with the caret's.*** The caret keys off the
    /// pipeline id itself, so the top bit is reserved to keep the two spaces apart.
    #[test]
    fn keys_stay_out_of_the_caret_key_space() {
        let pipeline = PipelineId(7, 3);
        let key: PropertyBindingKey<f32> =
            paint_animation_binding_key(pipeline, 0x1234_5678, PaintAnimatedProperty::Opacity);
        assert_eq!(key.id.namespace.0, 7);
        assert_ne!(key.id.uid, 3, "must not land on the pipeline's own uid");
        assert_ne!(key.id.uid & 0x8000_0000, 0);
    }

    /// The same element keeps the same key, so its binding survives from one display list
    /// to the next and WebRender keeps applying values to it. Different elements, and the
    /// same element's different properties, do not share one.
    #[test]
    fn keys_are_stable_and_distinct() {
        let pipeline = PipelineId(1, 1);
        let opacity: PropertyBindingKey<f32> =
            paint_animation_binding_key(pipeline, 42, PaintAnimatedProperty::Opacity);
        let again: PropertyBindingKey<f32> =
            paint_animation_binding_key(pipeline, 42, PaintAnimatedProperty::Opacity);
        let other_node: PropertyBindingKey<f32> =
            paint_animation_binding_key(pipeline, 43, PaintAnimatedProperty::Opacity);
        let other_property: PropertyBindingKey<f32> =
            paint_animation_binding_key(pipeline, 42, PaintAnimatedProperty::Transform);
        assert_eq!(opacity.id, again.id);
        assert_ne!(opacity.id, other_node.id);
        assert_ne!(opacity.id, other_property.id);
    }

    /// The easing must hit both ends exactly, or a fade would never reach 0 or 1.
    #[test]
    fn easing_is_exact_at_the_ends() {
        for easing in [
            PaintAnimationEasing::Linear,
            PaintAnimationEasing::CubicBezier(0.25, 0.1, 0.25, 1.0),
            PaintAnimationEasing::Steps(4, false),
        ] {
            assert_eq!(easing.ease(0.0), 0.0, "{easing:?} at 0");
            assert_eq!(easing.ease(1.0), 1.0, "{easing:?} at 1");
        }
    }

    /// `ease` is the CSS default and must actually curve: an implementation that silently
    /// fell back to linear would look right in a still frame and wrong in motion.
    #[test]
    fn the_default_easing_curves() {
        let ease = PaintAnimationEasing::CubicBezier(0.25, 0.1, 0.25, 1.0);
        let midpoint = ease.ease(0.5);
        assert!(
            midpoint > 0.55,
            "ease() should be ahead of linear at the midpoint, got {midpoint}"
        );
        assert!(ease.ease(0.25) < ease.ease(0.75), "must be monotonic");
    }

    /// ***A translate must survive the merge as one segment, and a rotate must not.***
    /// Matrix lerp and CSS transform-list interpolation agree for translation and
    /// disagree for rotation; the merge is what keeps the disagreement below the
    /// tolerance, by refusing to collapse what is not straight in matrix space.
    #[test]
    fn a_translation_collapses_but_a_rotation_does_not() {
        let step = 1.0 / 60.0;
        let translations: Vec<LayoutTransform> = (0..=60)
            .map(|index| LayoutTransform::translation(index as f32, 0.0, 0.0))
            .collect();
        assert_eq!(
            PaintAnimationSegment::<LayoutTransform>::from_samples(&translations, step).len(),
            1,
            "a straight translate is one segment"
        );

        let rotations: Vec<LayoutTransform> = (0..=60)
            .map(|index| {
                LayoutTransform::rotation(
                    0.0,
                    0.0,
                    1.0,
                    euclid::Angle::degrees(index as f32 * 3.0),
                )
            })
            .collect();
        let segments = PaintAnimationSegment::<LayoutTransform>::from_samples(&rotations, step);
        assert!(
            segments.len() > 1,
            "a rotation is not straight in matrix space, got {} segment(s)",
            segments.len()
        );
        // And what survives is within tolerance of the exact samples.
        for (index, sample) in rotations.iter().enumerate() {
            let time = index as f64 * step;
            let segment = segments
                .iter()
                .find(|segment| time < segment.end)
                .unwrap_or_else(|| segments.last().expect("no segments"));
            let span = segment.end - segment.start;
            let progress = if span > 0.0 {
                ((time - segment.start) / span).clamp(0.0, 1.0) as f32
            } else {
                1.0
            };
            let from = segment.from.to_array();
            let to = segment.to.to_array();
            for (slot, exact) in sample.to_array().iter().enumerate() {
                let replayed = from[slot] + (to[slot] - from[slot]) * progress;
                assert!(
                    (replayed - exact).abs() <= PaintAnimationSegment::<LayoutTransform>::EPSILON,
                    "sample {index} slot {slot}: replayed {replayed}, exact {exact}"
                );
            }
        }
    }

    /// A transform that never moves produces nothing to play, for the same reason as the
    /// scalar case: a bound property with no values behind it keeps whatever it last had.
    #[test]
    fn a_constant_transform_produces_nothing_to_play() {
        let samples = vec![LayoutTransform::translation(3.0, 4.0, 0.0); 61];
        assert!(
            PaintAnimationSegment::<LayoutTransform>::from_samples(&samples, 1.0 / 60.0).is_empty()
        );
    }

    /// Progress outside the segment clamps rather than extrapolating, so a late frame
    /// cannot push a value past its endpoint.
    #[test]
    fn easing_clamps_outside_the_unit_interval() {
        let easing = PaintAnimationEasing::Linear;
        assert_eq!(easing.ease(-1.0), 0.0);
        assert_eq!(easing.ease(2.0), 1.0);
    }
}
