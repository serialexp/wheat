//! A diff well: line rows with old and new numbers, washed by kind, or the
//! opaque card for what a line diff can't show.

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{
    Ink, List, ListState, PhosphorIcon, SelectionMode, StyleResolver, TextAlign, TextSize, Tracking,
};

use crate::app::Frame;
use crate::chrome;
use crate::model::{Diff, LineKind};

const LINE_H: f32 = 17.0;
const NUMBER_W: f32 = 38.0;
const WELL: [f32; 4] = [0.0, 0.0, 0.0, 0.32];

/// Draws `diff` filling `rect`, scrolled by `state`.
pub fn draw(
    frame: &mut Frame<'_>,
    rect: Rect,
    diff: &Diff,
    state: &mut ListState,
    icon: PhosphorIcon,
) {
    let theme = frame.theme;
    let s = StyleResolver::new(theme);
    frame
        .list
        .quad(rect.x, rect.y, rect.width, rect.height, WELL);
    frame
        .list
        .quad(rect.x, rect.y, rect.width, 2.0, [0.0, 0.0, 0.0, 0.25]);
    match diff {
        Diff::None | Diff::Loading => {}
        Diff::Failed(error) => {
            chrome::sans(
                frame.list,
                &s,
                Rect::new(rect.x + 16.0, rect.y + 16.0, rect.width - 32.0, 18.0),
                error,
                TextSize::Row,
                theme.danger_text,
            );
        }
        Diff::Opaque { kind, facts } => {
            let cx = rect.x + rect.width * 0.5;
            let mut y = rect.y + 64.0;
            frame.list.phosphor_icon(
                Rect::new(cx - 12.0, y, 24.0, 24.0),
                icon,
                s.ink(Ink::Disabled),
            );
            y += 34.0;
            let caption = s
                .caption_block(
                    &format!("{kind} · opaque"),
                    rect.x,
                    y,
                    Tracking::Section,
                    Ink::Label,
                )
                .with_max_width(rect.width)
                .with_align(TextAlign::Center);
            frame.list.text(caption);
            y += 22.0;
            let key_w = 120.0;
            for (key, value) in facts {
                chrome::mono_right(
                    frame.list,
                    &s,
                    Rect::new(cx - key_w - 8.0, y, key_w, 16.0),
                    key,
                    TextSize::Dense,
                    s.ink(Ink::Caption),
                );
                chrome::mono(
                    frame.list,
                    &s,
                    Rect::new(cx + 8.0, y, rect.right() - cx - 24.0, 16.0),
                    value,
                    TextSize::Dense,
                    s.ink(Ink::Second),
                );
                y += 18.0;
            }
        }
        Diff::Text { lines } => {
            let added = chrome::fade(theme.status_ok, 0.13);
            let removed = chrome::fade(theme.danger_text, 0.14);
            let hunk = [1.0, 1.0, 1.0, 0.03];
            let number_ink = s.ink(Ink::Disabled);
            let inner = Rect::new(rect.x, rect.y + 4.0, rect.width, rect.height - 4.0);
            List::new()
                .with_item_height(LINE_H)
                .selection(SelectionMode::None)
                .draw(
                    inner,
                    lines.len(),
                    state,
                    frame.list,
                    &s,
                    frame.input,
                    |list, row, item| {
                        let line = &lines[item.index];
                        let (wash, ink, sign) = match line.kind {
                            LineKind::Added => (Some(added), theme.status_ok, "+ "),
                            LineKind::Removed => (Some(removed), theme.danger_text, "− "),
                            LineKind::Hunk => (Some(hunk), s.ink(Ink::Caption), ""),
                            LineKind::Context => (None, s.ink(Ink::Cell), "  "),
                        };
                        if let Some(wash) = wash {
                            list.quad(row.x, row.y, row.width, row.height, wash);
                        }
                        let number =
                            |value: Option<u32>| value.map(|n| n.to_string()).unwrap_or_default();
                        chrome::mono_right(
                            list,
                            &s,
                            Rect::new(row.x, row.y, NUMBER_W - 6.0, row.height),
                            &number(line.old),
                            TextSize::Dense,
                            number_ink,
                        );
                        chrome::mono_right(
                            list,
                            &s,
                            Rect::new(row.x + NUMBER_W, row.y, NUMBER_W - 10.0, row.height),
                            &number(line.new),
                            TextSize::Dense,
                            number_ink,
                        );
                        let text_x = row.x + 2.0 * NUMBER_W;
                        chrome::mono(
                            list,
                            &s,
                            Rect::new(
                                text_x,
                                row.y,
                                (row.right() - text_x - 12.0).max(0.0),
                                row.height,
                            ),
                            &format!("{sign}{}", line.text),
                            TextSize::Dense,
                            ink,
                        );
                    },
                );
        }
    }
}
