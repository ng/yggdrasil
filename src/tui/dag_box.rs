//! Box-drawing DAG renderer (yggdrasil-170). State-aware edge styling:
//! light box-drawing (`─│┌┐└┘`) for normal deps, heavy (`━┃┏┓┗┛`) for
//! the selected node's path, dashed (`╌╎`) for blocked deps,
//! double-line (`═║╔╗╚╝`) for the critical path.
//!
//! This module ships the glyph picker indexed by edge state. The
//! renderer that walks `arc_diagram::NodePosition` data and emits the
//! actual drawing layer composes on top.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeState {
    /// Default — normal dep edge, no special highlight.
    Normal,
    /// Edge sits on the selected node's path; render heavy.
    Selected,
    /// Blocker is closed but the dependent isn't — visually dashed
    /// because it's "still binding but should clear".
    Pending,
    /// Edge sits on the critical-path chain (yggdrasil-162).
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeShape {
    Horizontal,
    Vertical,
    CornerNE,
    CornerNW,
    CornerSE,
    CornerSW,
}

/// Pick the box-drawing glyph for a (state, shape) pair.
pub fn glyph_for(state: EdgeState, shape: EdgeShape) -> char {
    match (state, shape) {
        (EdgeState::Normal, EdgeShape::Horizontal) => '─',
        (EdgeState::Normal, EdgeShape::Vertical) => '│',
        (EdgeState::Normal, EdgeShape::CornerNE) => '┐',
        (EdgeState::Normal, EdgeShape::CornerNW) => '┌',
        (EdgeState::Normal, EdgeShape::CornerSE) => '┘',
        (EdgeState::Normal, EdgeShape::CornerSW) => '└',
        (EdgeState::Selected, EdgeShape::Horizontal) => '━',
        (EdgeState::Selected, EdgeShape::Vertical) => '┃',
        (EdgeState::Selected, EdgeShape::CornerNE) => '┓',
        (EdgeState::Selected, EdgeShape::CornerNW) => '┏',
        (EdgeState::Selected, EdgeShape::CornerSE) => '┛',
        (EdgeState::Selected, EdgeShape::CornerSW) => '┗',
        (EdgeState::Pending, EdgeShape::Horizontal) => '╌',
        (EdgeState::Pending, EdgeShape::Vertical) => '╎',
        // Dashed edges don't ship corner glyphs in standard Unicode;
        // fall back to the light corners for shape, the styling
        // (Modifier::DIM) communicates "pending" at the renderer.
        (EdgeState::Pending, EdgeShape::CornerNE) => '┐',
        (EdgeState::Pending, EdgeShape::CornerNW) => '┌',
        (EdgeState::Pending, EdgeShape::CornerSE) => '┘',
        (EdgeState::Pending, EdgeShape::CornerSW) => '└',
        (EdgeState::Critical, EdgeShape::Horizontal) => '═',
        (EdgeState::Critical, EdgeShape::Vertical) => '║',
        (EdgeState::Critical, EdgeShape::CornerNE) => '╗',
        (EdgeState::Critical, EdgeShape::CornerNW) => '╔',
        (EdgeState::Critical, EdgeShape::CornerSE) => '╝',
        (EdgeState::Critical, EdgeShape::CornerSW) => '╚',
    }
}

/// Pick the node-frame glyph (corner + side) for a given state.
/// Selected nodes get heavy frames; critical-path nodes get
/// double-line frames; everyone else light.
pub fn node_frame(state: EdgeState) -> NodeFrame {
    match state {
        EdgeState::Critical => NodeFrame {
            tl: '╔',
            tr: '╗',
            bl: '╚',
            br: '╝',
            h: '═',
            v: '║',
        },
        EdgeState::Selected => NodeFrame {
            tl: '┏',
            tr: '┓',
            bl: '┗',
            br: '┛',
            h: '━',
            v: '┃',
        },
        _ => NodeFrame {
            tl: '┌',
            tr: '┐',
            bl: '└',
            br: '┘',
            h: '─',
            v: '│',
        },
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NodeFrame {
    pub tl: char,
    pub tr: char,
    pub bl: char,
    pub br: char,
    pub h: char,
    pub v: char,
}
