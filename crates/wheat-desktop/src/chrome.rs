//! The comp's recurring pieces: the caps header strip over a pane, the 36 px
//! toolbar band over a screen, pane surfaces, rules, and single-line text cut
//! to a width.

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{DrawList, Ink, StyleResolver, TextAlign, TextBlock, TextSize, Theme, Tracking};

/// The caps strip over a pane: "Local edits", "Workspaces".
pub const HEADER_H: f32 = 22.0;
/// The band at the top of a screen, under the menu bar.
pub const TOOLBAR_H: f32 = 36.0;

/// A pane's body surface.
pub fn pane(list: &mut DrawList, theme: &Theme, rect: Rect) {
    list.paint_quad(rect, theme.chrome.dock.body);
}

/// A caps header strip with its `title`, a dim `aside` after it, and the rest
/// of the strip for the caller (a checkbox at the right). Returns the strip.
pub fn header(
    list: &mut DrawList,
    s: &StyleResolver,
    theme: &Theme,
    rect: Rect,
    title: &str,
    aside: &str,
) -> Rect {
    list.paint_quad(rect, theme.chrome.dock.header);
    let x = rect.x + 9.0;
    let block = s
        .caption_block(title, x, rect.y, Tracking::Section, Ink::Label)
        .with_line_height(rect.height);
    let width = list.measure_block(&block).0;
    list.text(block);
    if !aside.is_empty() {
        list.text(
            s.mono_block(aside, x + width + 8.0, rect.y, TextSize::Caption, Ink::Dim)
                .with_line_height(rect.height),
        );
    }
    rect
}

/// The toolbar band at the top of a screen.
pub fn toolbar(list: &mut DrawList, theme: &Theme, rect: Rect) {
    list.paint_quad(rect, theme.chrome.dock.header);
    hrule(list, theme, rect.x, rect.bottom() - 1.0, rect.width);
}

/// A one-pixel hard edge across.
pub fn hrule(list: &mut DrawList, theme: &Theme, x: f32, y: f32, width: f32) {
    list.quad(x, y, width, 1.0, theme.edge_hard);
}

/// A one-pixel hard edge down, with the faint highlight beside it.
pub fn vrule(list: &mut DrawList, theme: &Theme, x: f32, y: f32, height: f32) {
    list.quad(x, y, 1.0, height, theme.edge_hard);
    list.quad(x + 1.0, y, 1.0, height, [1.0, 1.0, 1.0, 0.035]);
}

/// Sans text on one line in `rect`, cut with an ellipsis at its width.
pub fn sans(
    list: &mut DrawList,
    s: &StyleResolver,
    rect: Rect,
    text: &str,
    size: TextSize,
    color: [f32; 4],
) -> f32 {
    line(
        list,
        s.sans_block(text, rect.x, rect.y, size, Ink::Row),
        rect,
        color,
    )
}

/// Mono text on one line in `rect`, cut with an ellipsis at its width.
pub fn mono(
    list: &mut DrawList,
    s: &StyleResolver,
    rect: Rect,
    text: &str,
    size: TextSize,
    color: [f32; 4],
) -> f32 {
    line(
        list,
        s.mono_block(text, rect.x, rect.y, size, Ink::Row),
        rect,
        color,
    )
}

/// Mono text right-aligned in `rect`.
pub fn mono_right(
    list: &mut DrawList,
    s: &StyleResolver,
    rect: Rect,
    text: &str,
    size: TextSize,
    color: [f32; 4],
) {
    list.text(
        s.mono_block(text, rect.x, rect.y, size, Ink::Row)
            .with_color_f32(color)
            .with_line_height(rect.height)
            .with_max_width(rect.width)
            .with_align(TextAlign::End),
    );
}

/// Caps text, as in a header strip, on one line in `rect`.
pub fn caps(
    list: &mut DrawList,
    s: &StyleResolver,
    rect: Rect,
    text: &str,
    color: [f32; 4],
) -> f32 {
    let block = s
        .caption_block(text, rect.x, rect.y, Tracking::Caption, Ink::Label)
        .with_color_f32(color)
        .with_line_height(rect.height);
    let width = list.measure_block(&block).0;
    list.text(block);
    width
}

/// Draws `block` on one line centred in `rect`'s height, cut at its width, and
/// returns the width it takes.
fn line(list: &mut DrawList, block: TextBlock, rect: Rect, color: [f32; 4]) -> f32 {
    let block = block
        .with_color_f32(color)
        .with_line_height(rect.height)
        .with_max_width(rect.width.max(0.0))
        .with_ellipsis();
    let width = list.measure_block(&block).0.min(rect.width.max(0.0));
    list.text(block);
    width
}

/// `color` at `alpha` of its own opacity.
pub fn fade(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], color[3] * alpha]
}
