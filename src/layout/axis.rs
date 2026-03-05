use niri_config::MainAxis;
use smithay::utils::{Coordinate, Logical, Point, Rectangle, Size};

use crate::utils::ResizeEdge;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisMap {
    main_axis: MainAxis,
}

impl AxisMap {
    pub const fn new(main_axis: MainAxis) -> Self {
        Self { main_axis }
    }

    pub const fn main_axis(self) -> MainAxis {
        self.main_axis
    }

    pub const fn is_vertical(self) -> bool {
        matches!(self.main_axis, MainAxis::Vertical)
    }

    pub fn point_in<N: Coordinate>(self, point: Point<N, Logical>) -> Point<N, Logical> {
        if self.is_vertical() {
            Point::from((point.y, point.x))
        } else {
            point
        }
    }

    pub fn point_out<N: Coordinate>(self, point: Point<N, Logical>) -> Point<N, Logical> {
        self.point_in(point)
    }

    pub fn size_in<N: Coordinate>(self, size: Size<N, Logical>) -> Size<N, Logical> {
        if self.is_vertical() {
            Size::from((size.h, size.w))
        } else {
            size
        }
    }

    pub fn size_out<N: Coordinate>(self, size: Size<N, Logical>) -> Size<N, Logical> {
        self.size_in(size)
    }

    pub fn rect_in<N: Coordinate>(self, rect: Rectangle<N, Logical>) -> Rectangle<N, Logical> {
        Rectangle::new(self.point_in(rect.loc), self.size_in(rect.size))
    }

    pub fn rect_out<N: Coordinate>(self, rect: Rectangle<N, Logical>) -> Rectangle<N, Logical> {
        self.rect_in(rect)
    }

    pub fn point_from_main_cross<N: Coordinate>(self, main: N, cross: N) -> Point<N, Logical> {
        self.point_out(Point::from((main, cross)))
    }

    pub fn size_from_main_cross<N: Coordinate>(self, main: N, cross: N) -> Size<N, Logical> {
        self.size_out(Size::from((main, cross)))
    }

    pub fn resize_edges_in(self, edges: ResizeEdge) -> ResizeEdge {
        if !self.is_vertical() {
            return edges;
        }

        let mut mapped = ResizeEdge::empty();
        if edges.contains(ResizeEdge::LEFT) {
            mapped |= ResizeEdge::TOP;
        }
        if edges.contains(ResizeEdge::RIGHT) {
            mapped |= ResizeEdge::BOTTOM;
        }
        if edges.contains(ResizeEdge::TOP) {
            mapped |= ResizeEdge::LEFT;
        }
        if edges.contains(ResizeEdge::BOTTOM) {
            mapped |= ResizeEdge::RIGHT;
        }
        mapped
    }

    pub fn resize_edges_out(self, edges: ResizeEdge) -> ResizeEdge {
        self.resize_edges_in(edges)
    }
}
