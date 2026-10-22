//! The workspaces sidebar: every workspace attached to the store, agents'
//! included, then a summary of the store.

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{
    Ink, List, ListState, PhosphorIcon, SelectionMode, Status, StyleResolver, TextSize,
    color::oklch, status_dot,
};

use crate::app::Frame;
use crate::chrome::{self, HEADER_H};
use crate::model::{Model, WorkspaceState};

const ROW_H: f32 = 54.0;
const THUMB: f32 = 26.0;
const STORE_H: f32 = 70.0;

#[derive(Default)]
pub struct Sidebar {
    list: ListState,
}

impl Sidebar {
    pub fn draw(&mut self, frame: &mut Frame<'_>, rect: Rect, model: &Model) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::pane(frame.list, theme, rect);
        chrome::vrule(frame.list, theme, rect.right() - 1.0, rect.y, rect.height);
        let top = Rect::new(rect.x, rect.y, rect.width - 1.0, HEADER_H);
        chrome::header(
            frame.list,
            &s,
            theme,
            top,
            "Workspaces",
            &model.workspaces.len().to_string(),
        );
        chrome::mono_right(
            frame.list,
            &s,
            Rect::new(top.x, top.y, top.width - 8.0, top.height),
            "one store",
            TextSize::Caption,
            s.ink(Ink::Dim),
        );
        let store_top = rect.bottom() - STORE_H - HEADER_H;
        let rows = Rect::new(
            rect.x,
            top.bottom(),
            top.width,
            (store_top - top.bottom()).max(0.0),
        );
        List::new()
            .with_item_height(ROW_H)
            .selection(SelectionMode::None)
            .draw(
                rows,
                model.workspaces.len(),
                &mut self.list,
                frame.list,
                &s,
                frame.input,
                |list, row, item| {
                    let workspace = &model.workspaces[item.index];
                    list.quad(
                        row.x,
                        row.bottom() - 1.0,
                        row.width,
                        1.0,
                        [0.0, 0.0, 0.0, 0.35],
                    );
                    let thumb = Rect::new(row.x + 10.0, row.y + 7.0, THUMB, THUMB);
                    let tint = match workspace.agent {
                        Some(agent) => oklch(0.7, 0.09, agent.hue, 1.0),
                        None => oklch(0.78, 0.03, 240.0, 1.0),
                    };
                    list.rounded_rect(thumb, 1.0, tint);
                    list.rect_outline(thumb, 1.0, theme.edge_hard);
                    let icon = if workspace.agent.is_some() {
                        PhosphorIcon::Robot
                    } else {
                        PhosphorIcon::User
                    };
                    list.phosphor_icon(thumb.inset(6.5), icon, [0.043, 0.055, 0.063, 1.0]);
                    let x = thumb.right() + 9.0;
                    let width = (row.right() - 10.0 - x).max(0.0);
                    let row_ink = s.ink(Ink::Row);
                    let name_w = chrome::sans(
                        list,
                        &s,
                        Rect::new(x, row.y + 6.0, width - 40.0, 15.0),
                        &workspace.name,
                        TextSize::Row,
                        row_ink,
                    );
                    if workspace.current {
                        chrome::mono(
                            list,
                            &s,
                            Rect::new(x + name_w + 6.0, row.y + 6.0, 30.0, 15.0),
                            "this",
                            TextSize::Caption,
                            chrome::fade(row_ink, 0.7),
                        );
                    }
                    let status = match workspace.state {
                        WorkspaceState::Running => Status::Running,
                        WorkspaceState::Waiting => Status::Waiting,
                        WorkspaceState::Idle => Status::Idle,
                    };
                    status_dot(list, &s, (row.right() - 14.0, row.y + 13.5), status);
                    let change = workspace
                        .selected
                        .and_then(|index| model.changes.get(index))
                        .map(|change| format!(" · {} {}", change.short, change.title))
                        .unwrap_or_default();
                    chrome::mono(
                        list,
                        &s,
                        Rect::new(x, row.y + 21.0, width, 14.0),
                        &format!("{}{change}", workspace.frontier),
                        TextSize::Meta,
                        chrome::fade(row_ink, 0.72),
                    );
                    if let Some(task) = &workspace.task {
                        chrome::sans(
                            list,
                            &s,
                            Rect::new(x, row.y + 35.0, width, 14.0),
                            task,
                            TextSize::Meta,
                            chrome::fade(row_ink, 0.6),
                        );
                    }
                },
            );
        let store = Rect::new(rect.x, store_top, top.width, HEADER_H);
        chrome::hrule(frame.list, theme, store.x, store.y - 1.0, store.width);
        chrome::header(frame.list, &s, theme, store, "Store", "");
        let summary = &model.store;
        let lines = [
            ("changes", group_digits(summary.changes)),
            (
                "unpub.",
                format!(
                    "{} revisions · {} changes",
                    summary.unpublished_revisions, summary.unpublished_changes
                ),
            ),
            (
                "conflicts",
                format!(
                    "{} changes · {} paths",
                    summary.conflicted_changes, summary.conflicted_paths
                ),
            ),
        ];
        let mut y = store.bottom() + 8.0;
        for (key, value) in lines {
            chrome::sans(
                frame.list,
                &s,
                Rect::new(rect.x + 10.0, y, 64.0, 15.0),
                key,
                TextSize::Dense,
                s.ink(Ink::Caption),
            );
            chrome::mono(
                frame.list,
                &s,
                Rect::new(rect.x + 74.0, y, rect.width - 84.0, 15.0),
                &value,
                TextSize::Dense,
                s.ink(Ink::Second),
            );
            y += 19.0;
        }
    }
}

/// `2184` as `2,184`.
pub fn group_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
