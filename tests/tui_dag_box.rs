//! Regression for the box-drawing DAG glyph picker (yggdrasil-170).

use ygg::tui::dag_box::{EdgeShape, EdgeState, glyph_for, node_frame};

#[test]
fn normal_edges_use_light_box_drawing() {
    assert_eq!(glyph_for(EdgeState::Normal, EdgeShape::Horizontal), '─');
    assert_eq!(glyph_for(EdgeState::Normal, EdgeShape::Vertical), '│');
    assert_eq!(glyph_for(EdgeState::Normal, EdgeShape::CornerNW), '┌');
}

#[test]
fn selected_edges_use_heavy_box_drawing() {
    assert_eq!(glyph_for(EdgeState::Selected, EdgeShape::Horizontal), '━');
    assert_eq!(glyph_for(EdgeState::Selected, EdgeShape::Vertical), '┃');
    assert_eq!(glyph_for(EdgeState::Selected, EdgeShape::CornerNW), '┏');
}

#[test]
fn pending_edges_use_dashed_lines() {
    assert_eq!(glyph_for(EdgeState::Pending, EdgeShape::Horizontal), '╌');
    assert_eq!(glyph_for(EdgeState::Pending, EdgeShape::Vertical), '╎');
}

#[test]
fn critical_path_uses_double_lines() {
    assert_eq!(glyph_for(EdgeState::Critical, EdgeShape::Horizontal), '═');
    assert_eq!(glyph_for(EdgeState::Critical, EdgeShape::Vertical), '║');
    assert_eq!(glyph_for(EdgeState::Critical, EdgeShape::CornerNW), '╔');
}

#[test]
fn node_frame_critical_uses_double_line_corners() {
    let f = node_frame(EdgeState::Critical);
    assert_eq!(f.tl, '╔');
    assert_eq!(f.h, '═');
}

#[test]
fn node_frame_selected_uses_heavy_corners() {
    let f = node_frame(EdgeState::Selected);
    assert_eq!(f.tl, '┏');
    assert_eq!(f.h, '━');
}

#[test]
fn node_frame_normal_uses_light_corners() {
    let f = node_frame(EdgeState::Normal);
    assert_eq!(f.tl, '┌');
    assert_eq!(f.h, '─');
}
