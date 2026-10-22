//! The Workspace screen: where this workspace stands, its local edits, a diff
//! of the picked file, and capturing edits into a change.

use std::collections::BTreeSet;

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{
    Button, Checkbox, FocusId, FocusState, Ink, InputState, List, ListState, PhosphorIcon,
    SelectionMode, Status, StyleResolver, Tabs, TextInput, TextSize, Tone, status_dot,
};

use crate::app::Frame;
use crate::backend::{NewBase, Request, Target};
use crate::chrome::{self, HEADER_H, TOOLBAR_H};
use crate::diff_view;
use crate::model::{Diff, FileCode, FileEntry, FileGroup, Model};

const FILES_W: f32 = 400.0;
const ROW_H: f32 = 22.0;
const MESSAGE_ID: FocusId = 0x5748_0101;
const CAPTURE_H: f32 = 166.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TargetTab {
    #[default]
    Selected,
    Other,
    New,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseTab {
    #[default]
    Frontier,
    Changes,
    Independent,
}

/// One row of the local-edits list: a group's caption or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Group(FileGroup, usize),
    File(usize),
}

pub struct WorkspaceScreen {
    pub checked: BTreeSet<String>,
    pub selected_path: Option<String>,
    pub show_ignored: bool,
    pub target: TargetTab,
    pub other: Option<usize>,
    pub base: BaseTab,
    pub message: TextInput,
    files: ListState,
    others: ListState,
    diff_scroll: ListState,
    diff: Diff,
    /// The rows as last laid out: groups and files, ignored ones only when shown.
    rows: Vec<Row>,
    /// Files per group, by `FileGroup as usize`.
    counts: [usize; 3],
    /// A checkbox to paint with: never focused, never clicked.
    idle_focus: FocusState,
    idle_input: InputState,
}

impl Default for WorkspaceScreen {
    fn default() -> Self {
        Self {
            checked: BTreeSet::new(),
            selected_path: None,
            show_ignored: false,
            target: TargetTab::default(),
            other: None,
            base: BaseTab::default(),
            message: TextInput::new(0.0, 0.0, 0.0, 0.0)
                .with_multiline(true)
                .with_placeholder("Message — the first line becomes the change title"),
            files: ListState::default(),
            others: ListState::default(),
            diff_scroll: ListState::default(),
            diff: Diff::None,
            rows: Vec::new(),
            counts: [0; 3],
            idle_focus: FocusState::default(),
            idle_input: InputState::default(),
        }
    }
}

impl WorkspaceScreen {
    pub fn loaded(&mut self, model: &Model, pending: &mut Vec<Request>) {
        let files = &model.workspace.files;
        self.checked.retain(|path| {
            files
                .iter()
                .any(|file| &file.path == path && file.group != FileGroup::Ignored)
        });
        let still_there = self
            .selected_path
            .as_ref()
            .is_some_and(|path| files.iter().any(|file| &file.path == path));
        if !still_there {
            self.selected_path = files
                .iter()
                .find(|file| file.group != FileGroup::Ignored)
                .map(|file| file.path.clone());
        }
        self.other = self.other.filter(|&index| index < model.changes.len());
        self.layout_rows(model);
        // A reload may follow an action that changed the file.
        self.request_diff(pending);
    }

    #[cfg(test)]
    pub fn diff(&self) -> &Diff {
        &self.diff
    }

    pub fn diff_arrived(&mut self, path: String, diff: Diff) {
        if self.selected_path.as_deref() == Some(path.as_str()) {
            self.diff = diff;
        }
    }

    fn request_diff(&mut self, pending: &mut Vec<Request>) {
        self.diff_scroll = ListState::default();
        match &self.selected_path {
            Some(path) => {
                self.diff = Diff::Loading;
                pending.push(Request::WorkspaceDiff { path: path.clone() });
            }
            None => self.diff = Diff::None,
        }
    }

    fn layout_rows(&mut self, model: &Model) {
        self.rows.clear();
        self.counts = [0; 3];
        for file in &model.workspace.files {
            self.counts[file.group as usize] += 1;
        }
        for group in [FileGroup::Tracked, FileGroup::Untracked, FileGroup::Ignored] {
            if group == FileGroup::Ignored && !self.show_ignored {
                continue;
            }
            let count = self.counts[group as usize];
            if count == 0 {
                continue;
            }
            self.rows.push(Row::Group(group, count));
            for (index, file) in model.workspace.files.iter().enumerate() {
                if file.group == group {
                    self.rows.push(Row::File(index));
                }
            }
        }
    }

    /// Captures the picked paths into the chosen change.
    pub fn add(&mut self, model: &Model, pending: &mut Vec<Request>) {
        if self.checked.is_empty() {
            return;
        }
        let target = match self.target {
            TargetTab::Selected => Target::Selected,
            TargetTab::Other => match self.other.and_then(|index| model.changes.get(index)) {
                Some(change) => Target::Change(change.id.clone()),
                None => return,
            },
            TargetTab::New => Target::New(match self.base {
                BaseTab::Frontier => NewBase::Frontier,
                BaseTab::Changes => NewBase::Changes(Vec::new()),
                BaseTab::Independent => NewBase::Independent,
            }),
        };
        pending.push(Request::Add {
            paths: std::mem::take(&mut self.checked).into_iter().collect(),
            target,
            message: self.message.value.clone(),
        });
        self.message.clear();
    }

    /// Puts the picked tracked paths back as the materialized tree has them.
    pub fn restore(&mut self, pending: &mut Vec<Request>) {
        if self.checked.is_empty() {
            return;
        }
        pending.push(Request::Restore {
            paths: std::mem::take(&mut self.checked).into_iter().collect(),
        });
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        frame
            .list
            .quad(rect.x, rect.y, rect.width, rect.height, theme.background);
        let bar = Rect::new(rect.x, rect.y, rect.width, TOOLBAR_H);
        self.draw_toolbar(frame, bar, model, pending);
        let mut top = bar.bottom();
        if model.workspace.frontier.is_none() && model.workspace.frontiers.len() > 1 {
            let banner = Rect::new(rect.x, top, rect.width, 38.0);
            self.draw_choice(frame, banner, model, pending);
            top = banner.bottom();
        }
        let files = Rect::new(rect.x, top, FILES_W, rect.bottom() - top);
        let diff = Rect::new(
            files.right(),
            top,
            rect.right() - files.right(),
            rect.bottom() - top,
        );
        self.draw_files(frame, files, model, pending);
        self.draw_diff(frame, diff, model);
    }

    fn draw_toolbar(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::toolbar(frame.list, theme, rect);
        let view = &model.workspace;
        let mut x = rect.x + 12.0;
        let field = |frame: &mut Frame<'_>,
                     x: &mut f32,
                     key: &str,
                     value: &str,
                     extra: &str,
                     extra_ink: [f32; 4]| {
            let w = chrome::caps(
                frame.list,
                &s,
                Rect::new(*x, rect.y, 80.0, rect.height),
                key,
                s.ink(Ink::Label),
            );
            *x += w + 7.0;
            let w = chrome::mono(
                frame.list,
                &s,
                Rect::new(*x, rect.y, 200.0, rect.height),
                value,
                TextSize::Row,
                s.ink(Ink::Emph),
            );
            *x += w;
            if !extra.is_empty() {
                *x += 7.0;
                let w = chrome::mono(
                    frame.list,
                    &s,
                    Rect::new(*x, rect.y, 340.0, rect.height),
                    extra,
                    TextSize::Meta,
                    extra_ink,
                );
                *x += w;
            }
            *x += 18.0;
        };
        field(frame, &mut x, "label", &view.label, "", s.ink(Ink::Dim));
        let frontiers = view.frontiers.len();
        let count = match (&view.frontier, frontiers) {
            (_, 0 | 1) => String::new(),
            (Some(_), n) => format!("1 of {n}"),
            (None, n) => format!("{n} below {}", view.label),
        };
        field(
            frame,
            &mut x,
            "frontier",
            view.frontier.as_deref().unwrap_or("none chosen"),
            &count,
            theme.warn_meta,
        );
        let selected = view.selected.and_then(|index| model.changes.get(index));
        let (short, title) = selected.map_or(("—", ""), |change| {
            (change.short.as_str(), change.title.as_str())
        });
        let caps_w = chrome::caps(
            frame.list,
            &s,
            Rect::new(x, rect.y, 80.0, rect.height),
            "change",
            s.ink(Ink::Label),
        );
        x += caps_w + 7.0;
        x += chrome::mono(
            frame.list,
            &s,
            Rect::new(x, rect.y, 60.0, rect.height),
            short,
            TextSize::Row,
            s.ink(Ink::Emph),
        ) + 7.0;
        let title_room = if view.moved.is_some() {
            320.0
        } else {
            (rect.right() - x - 12.0).max(0.0)
        };
        chrome::sans(
            frame.list,
            &s,
            Rect::new(x, rect.y, title_room, rect.height),
            title,
            TextSize::Menu,
            s.ink(Ink::Title),
        );
        if let Some(moved) = &view.moved {
            let button_w = Button::new("Refresh").intrinsic_size(frame.list, &s).0;
            let button = Rect::new(rect.right() - 12.0 - button_w, rect.y + 7.0, button_w, 22.0);
            if Button::new("Refresh").draw(button, &mut frame.cx()) {
                pending.push(Request::Refresh);
            }
            let meta_w = s.mono_width(frame.list, moved, TextSize::Meta);
            let meta_x = button.x - 8.0 - meta_w;
            chrome::mono(
                frame.list,
                &s,
                Rect::new(meta_x, rect.y, meta_w + 1.0, rect.height),
                moved,
                TextSize::Meta,
                s.ink(Ink::Dim),
            );
            let text = "Repository advanced since materialization";
            let text_w = s.sans_width(frame.list, text, TextSize::Row);
            let text_x = meta_x - 8.0 - text_w;
            chrome::sans(
                frame.list,
                &s,
                Rect::new(text_x, rect.y, text_w + 1.0, rect.height),
                text,
                TextSize::Row,
                s.ink(Ink::Body2),
            );
            status_dot(
                frame.list,
                &s,
                (text_x - 11.0, rect.y + rect.height * 0.5),
                Status::Waiting,
            );
        }
    }

    fn draw_choice(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        frame.list.quad(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            chrome::fade(theme.warn_meta, 0.10),
        );
        frame
            .list
            .quad(rect.x, rect.y, 3.0, rect.height, theme.warn_rule);
        chrome::hrule(frame.list, theme, rect.x, rect.bottom() - 1.0, rect.width);
        frame.list.phosphor_icon(
            Rect::new(rect.x + 12.0, rect.y + 12.0, 14.0, 14.0),
            PhosphorIcon::GitFork,
            theme.warn_meta,
        );
        let view = &model.workspace;
        let text = format!(
            "{} frontiers below {}. Choose one to materialize, or integrate them.",
            view.frontiers.len(),
            view.label
        );
        chrome::sans(
            frame.list,
            &s,
            Rect::new(rect.x + 38.0, rect.y, rect.width * 0.5, rect.height),
            &text,
            TextSize::Menu,
            theme.warn_banner_ink,
        );
        let mut x = rect.right() - 12.0;
        let integrate = Button::new("Integrate…").tone(Tone::Accent).enabled(false);
        let w = integrate.intrinsic_size(frame.list, &s).0;
        x -= w;
        integrate.draw(Rect::new(x, rect.y + 8.0, w, 22.0), &mut frame.cx());
        for choice in view.frontiers.iter().rev() {
            let label = match choice.changes {
                Some(changes) => format!(
                    "{} · {changes} change{}",
                    choice.name,
                    if changes == 1 { "" } else { "s" }
                ),
                None => choice.name.clone(),
            };
            let button = Button::new(label);
            let w = button.intrinsic_size(frame.list, &s).0;
            x -= w + 8.0;
            if button.draw(Rect::new(x, rect.y + 8.0, w, 22.0), &mut frame.cx()) {
                pending.push(Request::SwitchFrontier {
                    frontier: choice.name.clone(),
                });
            }
        }
    }

    fn draw_files(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::pane(frame.list, theme, rect);
        chrome::vrule(frame.list, theme, rect.right() - 1.0, rect.y, rect.height);
        let files = &model.workspace.files;
        let [tracked, untracked, _] = self.counts;
        let head = Rect::new(rect.x, rect.y, rect.width - 1.0, HEADER_H);
        chrome::header(
            frame.list,
            &s,
            theme,
            head,
            "Local edits",
            &format!("{tracked} tracked · {untracked} untracked"),
        );
        let toggle = Rect::new(head.right() - 70.0, head.y + 4.0, 64.0, 14.0);
        if Checkbox::new().draw(self.show_ignored, "ignored", toggle, &mut frame.cx()) {
            self.show_ignored = !self.show_ignored;
            self.layout_rows(model);
        }
        let capture_head = Rect::new(
            rect.x,
            rect.bottom() - CAPTURE_H - HEADER_H,
            head.width,
            HEADER_H,
        );
        let list_rect = Rect::new(
            rect.x,
            head.bottom(),
            head.width,
            (capture_head.y - head.bottom()).max(0.0),
        );
        let visible = self.rows.iter().any(|row| matches!(row, Row::File(_)));
        if !visible {
            let text = "Workspace matches the materialized tree.";
            frame.list.phosphor_icon(
                Rect::new(
                    list_rect.x + list_rect.width * 0.5 - 12.0,
                    list_rect.y + 48.0,
                    24.0,
                    24.0,
                ),
                PhosphorIcon::CheckCircle,
                s.ink(Ink::Disabled),
            );
            frame.list.text(
                s.sans_block(
                    text,
                    list_rect.x,
                    list_rect.y + 80.0,
                    TextSize::Menu,
                    Ink::Caption,
                )
                .with_max_width(list_rect.width)
                .with_align(wgpu_gameui::TextAlign::Center),
            );
        }
        let selected_path = self.selected_path.clone();
        let rows = &self.rows;
        let checked = &self.checked;
        let idle_focus = &mut self.idle_focus;
        let idle_input = &self.idle_input;
        let selected_row = rows.iter().position(|row| match row {
            Row::File(index) => selected_path.as_deref() == Some(files[*index].path.as_str()),
            Row::Group(..) => false,
        });
        self.files.sync_selection(selected_row);
        let disabled = |index: usize| matches!(rows[index], Row::Group(..));
        let output = List::new()
            .with_item_height(ROW_H)
            .with_zebra(true)
            .selection(SelectionMode::Single)
            .focused(true)
            .disabled(&disabled)
            .draw(
                list_rect,
                rows.len(),
                &mut self.files,
                frame.list,
                &s,
                frame.input,
                |list, row, item| match rows[item.index] {
                    Row::Group(group, count) => {
                        list.quad(row.x, row.y, row.width, row.height, [0.0, 0.0, 0.0, 0.18]);
                        list.quad(
                            row.x,
                            row.bottom() - 1.0,
                            row.width,
                            1.0,
                            [0.0, 0.0, 0.0, 0.35],
                        );
                        let title = match group {
                            FileGroup::Tracked => "Tracked",
                            FileGroup::Untracked => "Untracked",
                            FileGroup::Ignored => "Ignored",
                        };
                        let w = chrome::caps(
                            list,
                            &s,
                            Rect::new(row.x + 9.0, row.y, 100.0, row.height),
                            title,
                            s.ink(Ink::Caption),
                        );
                        chrome::mono(
                            list,
                            &s,
                            Rect::new(row.x + 17.0 + w, row.y, 60.0, row.height),
                            &count.to_string(),
                            TextSize::Caption,
                            s.ink(Ink::Dim),
                        );
                    }
                    Row::File(index) => {
                        let file = &files[index];
                        let on_accent = item.selected;
                        let ink = if on_accent {
                            theme.on_accent
                        } else {
                            s.ink(Ink::Row)
                        };
                        let dim = if file.group == FileGroup::Ignored {
                            0.6
                        } else {
                            1.0
                        };
                        let mut x = row.x + 10.0;
                        if file.group != FileGroup::Ignored {
                            let mut cx = wgpu_gameui::DrawContext::new(
                                list, idle_focus, theme, idle_input, 0.0, 0.0,
                            );
                            Checkbox::new().draw(
                                checked.contains(&file.path),
                                "",
                                Rect::new(x, row.y + 4.5, 13.0, 13.0),
                                &mut cx,
                            );
                            x += 21.0;
                        }
                        let code_ink = if on_accent {
                            theme.on_accent
                        } else {
                            code_ink(theme, &s, file.code)
                        };
                        chrome::mono(
                            list,
                            &s,
                            Rect::new(x, row.y, 12.0, row.height),
                            file.code.letter(),
                            TextSize::Meta,
                            chrome::fade(code_ink, dim),
                        );
                        x += 20.0;
                        let stat_w = if file.stat.is_empty() {
                            0.0
                        } else {
                            s.mono_width(list, &file.stat, TextSize::Meta) + 8.0
                        };
                        chrome::mono(
                            list,
                            &s,
                            Rect::new(
                                x,
                                row.y,
                                (row.right() - 10.0 - stat_w - x).max(0.0),
                                row.height,
                            ),
                            &file.path,
                            TextSize::Dense,
                            chrome::fade(ink, dim),
                        );
                        if !file.stat.is_empty() {
                            chrome::mono_right(
                                list,
                                &s,
                                Rect::new(row.x, row.y, row.width - 10.0, row.height),
                                &file.stat,
                                TextSize::Meta,
                                chrome::fade(ink, 0.7 * dim),
                            );
                        }
                    }
                },
            );
        if let Some(index) = output.clicked
            && let Row::File(file_index) = self.rows[index]
        {
            let file = &files[file_index];
            let on_box = frame.input.mouse_x < list_rect.x + 28.0;
            if on_box && file.group != FileGroup::Ignored && !self.checked.remove(&file.path) {
                self.checked.insert(file.path.clone());
            }
            if self.selected_path.as_deref() != Some(file.path.as_str()) {
                self.selected_path = Some(file.path.clone());
                self.request_diff(pending);
            }
        }
        self.draw_capture(frame, capture_head, model, pending);
    }

    fn draw_capture(
        &mut self,
        frame: &mut Frame<'_>,
        head: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::hrule(frame.list, theme, head.x, head.y - 1.0, head.width);
        let picked = self.checked.len();
        chrome::header(
            frame.list,
            &s,
            theme,
            head,
            "Capture",
            &format!("{picked} paths picked"),
        );
        let x = head.x + 12.0;
        let width = head.width - 24.0;
        let mut y = head.bottom() + 10.0;
        chrome::sans(
            frame.list,
            &s,
            Rect::new(x, y, 52.0, 22.0),
            "Add to",
            TextSize::Row,
            s.ink(Ink::Row),
        );
        let labels = ["Selected", "Other change", "New change"];
        let active = match self.target {
            TargetTab::Selected => 0,
            TargetTab::Other => 1,
            TargetTab::New => 2,
        };
        let tabs = Tabs::new(&labels).draw(
            Rect::new(x + 60.0, y, 270.0, 22.0),
            active,
            frame.list,
            &s,
            frame.input,
            None,
        );
        if let Some(clicked) = tabs.clicked {
            self.target = [TargetTab::Selected, TargetTab::Other, TargetTab::New][clicked];
        }
        y += 31.0;
        let detail = Rect::new(x + 60.0, y, width - 60.0, 32.0);
        match self.target {
            TargetTab::Selected => {
                if let Some(index) = model.workspace.selected
                    && let Some(change) = model.changes.get(index)
                {
                    let w = chrome::mono(
                        frame.list,
                        &s,
                        Rect::new(detail.x, detail.y, 60.0, 15.0),
                        &change.short,
                        TextSize::Row,
                        s.ink(Ink::Emph),
                    );
                    chrome::sans(
                        frame.list,
                        &s,
                        Rect::new(detail.x + w + 6.0, detail.y, detail.width - w - 6.0, 15.0),
                        &change.title,
                        TextSize::Row,
                        s.ink(Ink::Title),
                    );
                    let dependents = model.dependents(index);
                    let mut note = format!("becomes revision {}", change.revision_count + 1);
                    if !dependents.is_empty() {
                        let shorts: Vec<&str> = dependents
                            .iter()
                            .map(|&at| model.changes[at].short.as_str())
                            .collect();
                        note.push_str(&format!(
                            " · reapplies {} dependents ({})",
                            dependents.len(),
                            shorts.join(", ")
                        ));
                    }
                    chrome::mono(
                        frame.list,
                        &s,
                        Rect::new(detail.x, detail.y + 17.0, detail.width, 14.0),
                        &note,
                        TextSize::Meta,
                        s.ink(Ink::Caption),
                    );
                } else {
                    chrome::sans(
                        frame.list,
                        &s,
                        Rect::new(detail.x, detail.y, detail.width, 15.0),
                        "No change is selected in this workspace.",
                        TextSize::Row,
                        s.ink(Ink::Caption),
                    );
                }
            }
            TargetTab::Other => {
                let choices: Vec<usize> = (0..model.changes.len())
                    .filter(|&index| {
                        !model.changes[index].published && Some(index) != model.workspace.selected
                    })
                    .collect();
                let other = self.other;
                let well = Rect::new(detail.x, detail.y - 2.0, detail.width, 36.0);
                self.others
                    .sync_selection(choices.iter().position(|&index| Some(index) == other));
                let output = List::new()
                    .with_item_height(18.0)
                    .with_zebra(true)
                    .selection(SelectionMode::Single)
                    .focused(true)
                    .draw(
                        well,
                        choices.len(),
                        &mut self.others,
                        frame.list,
                        &s,
                        frame.input,
                        |list, row, item| {
                            let change = &model.changes[choices[item.index]];
                            let ink = if item.selected {
                                theme.on_accent
                            } else {
                                s.ink(Ink::Row)
                            };
                            chrome::mono(
                                list,
                                &s,
                                Rect::new(row.x + 8.0, row.y, 40.0, row.height),
                                &change.short,
                                TextSize::Meta,
                                ink,
                            );
                            chrome::sans(
                                list,
                                &s,
                                Rect::new(row.x + 50.0, row.y, row.width - 58.0, row.height),
                                &change.title,
                                TextSize::Row,
                                ink,
                            );
                        },
                    );
                if let Some(index) = output.clicked {
                    self.other = Some(choices[index]);
                }
            }
            TargetTab::New => {
                chrome::sans(
                    frame.list,
                    &s,
                    Rect::new(x, detail.y, 52.0, 22.0),
                    "Built on",
                    TextSize::Row,
                    s.ink(Ink::Row),
                );
                let labels = ["Frontier", "Changes…", "Independent"];
                let active = match self.base {
                    BaseTab::Frontier => 0,
                    BaseTab::Changes => 1,
                    BaseTab::Independent => 2,
                };
                let tabs = Tabs::new(&labels).draw(
                    Rect::new(detail.x, detail.y, 270.0, 22.0),
                    active,
                    frame.list,
                    &s,
                    frame.input,
                    None,
                );
                if let Some(clicked) = tabs.clicked {
                    self.base =
                        [BaseTab::Frontier, BaseTab::Changes, BaseTab::Independent][clicked];
                }
            }
        }
        y += 40.0;
        self.message.x = x;
        self.message.y = y;
        self.message.width = width;
        self.message.height = 50.0;
        self.message.draw(MESSAGE_ID, &mut frame.cx());
        y += 58.0;
        let tracked = self.counts[FileGroup::Tracked as usize];
        let is_tracked = |file: &&FileEntry| file.group == FileGroup::Tracked;
        let tracked_picked = model
            .workspace
            .files
            .iter()
            .filter(is_tracked)
            .filter(|file| self.checked.contains(&file.path))
            .count();
        let all = tracked > 0 && tracked_picked == tracked;
        if Checkbox::new().draw(
            all,
            "All tracked",
            Rect::new(x, y + 5.0, 90.0, 14.0),
            &mut frame.cx(),
        ) {
            for file in model.workspace.files.iter().filter(is_tracked) {
                if all {
                    self.checked.remove(&file.path);
                } else {
                    self.checked.insert(file.path.clone());
                }
            }
        }
        let add_label = match self.target {
            TargetTab::New => "Create Change".to_owned(),
            _ => format!("Add {picked} Path{}", if picked == 1 { "" } else { "s" }),
        };
        let can_add = picked > 0
            && match self.target {
                TargetTab::Selected => model.workspace.selected.is_some(),
                TargetTab::Other => self.other.is_some(),
                // Picking the changes to build on needs a picker this pass
                // doesn't have yet.
                TargetTab::New => {
                    self.base != BaseTab::Changes && !self.message.value.trim().is_empty()
                }
            };
        let add = Button::new(add_label).tone(Tone::Accent).enabled(can_add);
        let add_w = add.intrinsic_size(frame.list, &s).0;
        let add_rect = Rect::new(x + width - add_w, y, add_w, 24.0);
        if add.draw(add_rect, &mut frame.cx()) {
            self.add(model, pending);
        }
        let restore = Button::new("Restore").tone(Tone::Ghost).enabled(picked > 0);
        let restore_w = restore.intrinsic_size(frame.list, &s).0;
        if restore.draw(
            Rect::new(add_rect.x - 8.0 - restore_w, y, restore_w, 24.0),
            &mut frame.cx(),
        ) {
            self.restore(pending);
        }
    }

    fn draw_diff(&mut self, frame: &mut Frame<'_>, rect: Rect, model: &Model) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        let head = Rect::new(rect.x, rect.y, rect.width, HEADER_H);
        frame.list.paint_quad(head, theme.chrome.dock.header);
        let file = self
            .selected_path
            .as_ref()
            .and_then(|path| model.workspace.files.iter().find(|file| &file.path == path));
        let (path, meta, icon) = match file {
            None => ("—".to_owned(), String::new(), PhosphorIcon::File),
            Some(file) => (
                file.path.clone(),
                match (file.code, &self.diff) {
                    (FileCode::Untracked, _) => "untracked · new file".to_owned(),
                    (_, Diff::Opaque { kind, .. }) => kind.clone(),
                    _ => format!("vs materialized tree · {}", file.stat),
                },
                match file.code {
                    FileCode::Deleted => PhosphorIcon::FileX,
                    FileCode::TypeChanged => PhosphorIcon::TerminalWindow,
                    FileCode::Ignored => PhosphorIcon::EyeSlash,
                    _ => PhosphorIcon::FileImage,
                },
            ),
        };
        let w = chrome::mono(
            frame.list,
            &s,
            Rect::new(head.x + 9.0, head.y, head.width * 0.6, head.height),
            &path,
            TextSize::Meta,
            s.ink(Ink::Title),
        );
        chrome::mono(
            frame.list,
            &s,
            Rect::new(head.x + 17.0 + w, head.y, head.width * 0.35, head.height),
            &meta,
            TextSize::Meta,
            s.ink(Ink::Dim),
        );
        let well = Rect::new(rect.x, head.bottom(), rect.width, rect.height - HEADER_H);
        diff_view::draw(frame, well, &self.diff, &mut self.diff_scroll, icon);
    }
}

fn code_ink(theme: &wgpu_gameui::Theme, s: &StyleResolver, code: FileCode) -> [f32; 4] {
    match code {
        FileCode::Modified => theme.warn_meta,
        FileCode::Deleted => theme.danger_text,
        FileCode::TypeChanged => s.ink(Ink::Glyph),
        FileCode::Untracked => theme.status_ok,
        FileCode::Ignored => s.ink(Ink::Disabled),
    }
}
