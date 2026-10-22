//! The Changes screen: the change graph with each change's revisions, and the
//! picked change's details and revision diff.

use std::collections::BTreeSet;

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{
    Badge, BadgeTone, Button, FocusId, Ink, List, ListState, PhosphorIcon, Row as HeightRow,
    RowHeights, SearchField, SelectionMode, StyleResolver, TextInput, TextSize, Tone,
};

use crate::app::Frame;
use crate::backend::Request;
use crate::chrome::{self, HEADER_H, TOOLBAR_H};
use crate::diff_view;
use crate::graph::Graph;
use crate::model::{Diff, Model};
use crate::sidebar::group_digits;

const DETAIL_W: f32 = 540.0;
const CHANGE_H: f32 = 24.0;
const REVISION_H: f32 = 22.0;
const GRAPH_W: f32 = 72.0;
const LANE_X: f32 = 8.0;
const LANE_GAP: f32 = 13.0;
const AUTHOR_W: f32 = 84.0;
const REVS_W: f32 = 38.0;
const STATE_W: f32 = 62.0;
const AGE_W: f32 = 34.0;
const SEARCH_ID: FocusId = 0x5748_0201;

/// One row of the change list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Change(usize),
    /// A change's revision, by index into its `revisions`.
    Revision(usize, usize),
    /// The line counting the published changes not loaded.
    Older,
}

pub struct HistoryScreen {
    /// The picked change, by id so a reload keeps it.
    pub picked: Option<String>,
    /// The picked revision of the picked change; its latest when `None`.
    pub revision: Option<usize>,
    /// Changes showing their revisions, by id.
    pub expanded: BTreeSet<String>,
    /// Expanded changes whose revisions were asked for and haven't arrived, by id.
    fetching: BTreeSet<String>,
    pub search: TextInput,
    rows: Vec<Row>,
    heights: RowHeights,
    graph: Graph,
    /// The query `rows` was laid out for.
    query: String,
    list: ListState,
    diff_scroll: ListState,
    diff: Diff,
    /// The revision `diff` is for: change id and revision index.
    diff_for: Option<(String, usize)>,
}

impl Default for HistoryScreen {
    fn default() -> Self {
        Self {
            picked: None,
            revision: None,
            expanded: BTreeSet::new(),
            fetching: BTreeSet::new(),
            search: TextInput::new(0.0, 0.0, 0.0, 0.0)
                .with_placeholder("Find change by title, author or id"),
            rows: Vec::new(),
            heights: RowHeights::new(),
            graph: Graph::default(),
            query: String::new(),
            list: ListState::default(),
            diff_scroll: ListState::default(),
            diff: Diff::None,
            diff_for: None,
        }
    }
}

impl HistoryScreen {
    pub fn loaded(&mut self, model: &Model, pending: &mut Vec<Request>) {
        self.graph = Graph::layout(model);
        let still_there = self
            .picked
            .as_ref()
            .is_some_and(|id| model.changes.iter().any(|change| &change.id == id));
        if !still_there {
            self.picked = None;
            self.revision = None;
            self.expanded.clear();
            let first = model.workspace.selected.or(if model.changes.is_empty() {
                None
            } else {
                Some(0)
            });
            if let Some(index) = first {
                let id = model.changes[index].id.clone();
                self.expanded.insert(id.clone());
                self.picked = Some(id);
            }
        }
        self.expanded
            .retain(|id| model.changes.iter().any(|change| &change.id == id));
        // A reload carries each change's latest revision only.
        self.fetching.clear();
        self.fetch_revisions(model, pending);
        self.layout_rows(model);
        // The picked revision may have new content after a squash or capture.
        self.diff_for = None;
        self.request_diff(model, pending);
    }

    /// A change's revisions arrived in `model`: show them.
    pub fn revisions_arrived(&mut self, model: &Model, pending: &mut Vec<Request>) {
        self.fetching.retain(|id| {
            model
                .changes
                .iter()
                .any(|c| &c.id == id && !c.all_revisions_loaded())
        });
        self.layout_rows(model);
        self.request_diff(model, pending);
    }

    /// Asks for the revisions of every expanded change that has only its latest.
    fn fetch_revisions(&mut self, model: &Model, pending: &mut Vec<Request>) {
        for change in &model.changes {
            if self.expanded.contains(&change.id)
                && !change.all_revisions_loaded()
                && self.fetching.insert(change.id.clone())
            {
                pending.push(Request::Revisions {
                    change: change.id.clone(),
                });
            }
        }
    }

    #[cfg(test)]
    pub fn diff_for(&self) -> Option<&(String, usize)> {
        self.diff_for.as_ref()
    }

    pub fn diff_arrived(&mut self, change: String, revision: usize, diff: Diff) {
        if self.diff_for.as_ref() == Some(&(change, revision)) {
            self.diff = diff;
        }
    }

    fn picked_index(&self, model: &Model) -> Option<usize> {
        let id = self.picked.as_ref()?;
        model.changes.iter().position(|change| &change.id == id)
    }

    /// The picked revision, clamped to the picked change's.
    fn picked_revision(&self, model: &Model) -> Option<(usize, usize)> {
        let index = self.picked_index(model)?;
        let count = model.changes[index].revision_count;
        if count == 0 {
            return None;
        }
        Some((index, self.revision.unwrap_or(count - 1).min(count - 1)))
    }

    fn request_diff(&mut self, model: &Model, pending: &mut Vec<Request>) {
        let Some((index, revision)) = self.picked_revision(model) else {
            self.diff = Diff::None;
            self.diff_for = None;
            return;
        };
        let key = (model.changes[index].id.clone(), revision);
        if self.diff_for.as_ref() == Some(&key) {
            return;
        }
        self.diff_scroll = ListState::default();
        // An older revision is picked from its row, so it has loaded; until the
        // chain arrives after a reload, it waits as Loading.
        let Some(id) = model.changes[index].revision(revision) else {
            self.diff = Diff::Loading;
            self.diff_for = None;
            return;
        };
        self.diff = Diff::Loading;
        pending.push(Request::RevisionDiff {
            change: key.0.clone(),
            revision,
            id: id.id.clone(),
        });
        self.diff_for = Some(key);
    }

    /// The rows for the current query: every loaded change with its expanded
    /// revisions, or the matching changes alone.
    fn layout_rows(&mut self, model: &Model) {
        self.query = self.search.value.clone();
        let needle = self.query.trim().to_lowercase();
        self.rows.clear();
        for (index, change) in model.changes.iter().enumerate() {
            if !needle.is_empty() && !matches(change, &needle) {
                continue;
            }
            self.rows.push(Row::Change(index));
            if self.expanded.contains(&change.id) {
                for revision in (change.first_loaded_revision()..change.revision_count).rev() {
                    self.rows.push(Row::Revision(index, revision));
                }
            }
        }
        if needle.is_empty() && model.older_changes > 0 {
            self.rows.push(Row::Older);
        }
        self.heights.clear();
        self.heights.extend(self.rows.iter().map(|row| {
            HeightRow::measured(match row {
                Row::Change(_) | Row::Older => CHANGE_H,
                Row::Revision(..) => REVISION_H,
            })
        }));
    }

    fn pick(
        &mut self,
        model: &Model,
        index: usize,
        revision: Option<usize>,
        pending: &mut Vec<Request>,
    ) {
        self.picked = Some(model.changes[index].id.clone());
        self.revision = revision;
        self.request_diff(model, pending);
    }

    pub fn squash_change(&mut self, model: &Model, pending: &mut Vec<Request>) {
        if let Some(index) = self.picked_index(model) {
            pending.push(Request::SquashChange {
                change: model.changes[index].id.clone(),
            });
        }
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
        self.draw_toolbar(frame, bar, model);
        if self.search.value != self.query {
            self.layout_rows(model);
            self.list = ListState::default();
        }
        let body = rect.bottom() - bar.bottom();
        let list = Rect::new(rect.x, bar.bottom(), (rect.width - DETAIL_W).max(0.0), body);
        let detail = Rect::new(
            list.right(),
            bar.bottom(),
            rect.right() - list.right(),
            body,
        );
        self.draw_list(frame, list, model, pending);
        self.draw_detail(frame, detail, model, pending);
    }

    fn draw_toolbar(&mut self, frame: &mut Frame<'_>, rect: Rect, model: &Model) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::toolbar(frame.list, theme, rect);
        let w = chrome::sans(
            frame.list,
            &s,
            Rect::new(rect.x + 12.0, rect.y, 120.0, rect.height),
            "Changes",
            TextSize::Menu,
            s.ink(Ink::Title),
        );
        let store = &model.store;
        let mut summary = format!("{} changes", group_digits(store.changes));
        if store.unpublished_changes > 0 {
            summary.push_str(&format!(" · {} unpublished", store.unpublished_changes));
        }
        if store.conflicted_changes > 0 {
            summary.push_str(&format!(" · {} conflicted", store.conflicted_changes));
        }
        chrome::mono(
            frame.list,
            &s,
            Rect::new(rect.x + 24.0 + w, rect.y, 400.0, rect.height),
            &summary,
            TextSize::Meta,
            s.ink(Ink::Dim),
        );
        let field = Rect::new(rect.right() - 12.0 - 280.0, rect.y + 6.0, 280.0, 24.0);
        SearchField::new().draw(&mut self.search, SEARCH_ID, field, &mut frame.cx());
    }

    fn draw_list(
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
        let head = Rect::new(rect.x, rect.y, rect.width - 1.0, HEADER_H);
        frame.list.paint_quad(head, theme.chrome.dock.header);
        let columns = Columns::new(head);
        let caption = s.ink(Ink::Label);
        chrome::caps(
            frame.list,
            &s,
            Rect::new(head.x + 9.0, head.y, GRAPH_W, head.height),
            "graph",
            caption,
        );
        chrome::caps(
            frame.list,
            &s,
            Rect::new(columns.title, head.y, 80.0, head.height),
            "change",
            caption,
        );
        chrome::caps(
            frame.list,
            &s,
            Rect::new(columns.author, head.y, AUTHOR_W, head.height),
            "author",
            caption,
        );
        chrome::caps(
            frame.list,
            &s,
            Rect::new(columns.revs, head.y, REVS_W, head.height),
            "revs",
            caption,
        );
        chrome::caps(
            frame.list,
            &s,
            Rect::new(columns.state, head.y, STATE_W, head.height),
            "state",
            caption,
        );
        chrome::caps(
            frame.list,
            &s,
            Rect::new(columns.age, head.y, AGE_W, head.height),
            "age",
            caption,
        );
        let body = Rect::new(
            rect.x,
            head.bottom(),
            head.width,
            rect.bottom() - head.bottom(),
        );
        let picked = self.picked_revision(model);
        let picked_row = self.rows.iter().position(|row| match (row, picked) {
            (Row::Change(index), Some((at, _))) => {
                *index == at && !self.expanded.contains(&model.changes[at].id)
            }
            (Row::Revision(index, revision), Some((at, rev))) => *index == at && *revision == rev,
            _ => false,
        });
        self.list.sync_selection(picked_row);
        let rows = &self.rows;
        let graph = &self.graph;
        let expanded = &self.expanded;
        let filtered = !self.query.trim().is_empty();
        let older = model.older_changes;
        let output = List::new()
            .selection(SelectionMode::Single)
            .focused(true)
            .draw_measured(
                body,
                &mut self.heights,
                |index, _, _| match rows[index] {
                    Row::Revision(..) => REVISION_H,
                    _ => CHANGE_H,
                },
                &mut self.list,
                frame.list,
                &s,
                frame.input,
                |list, row, item| {
                    let ink = if item.selected { theme.on_accent } else { s.ink(Ink::Row) };
                    let dim = if item.selected { chrome::fade(theme.on_accent, 0.75) } else { s.ink(Ink::Dim) };
                    let columns = Columns::new(row);
                    match rows[item.index] {
                        Row::Change(index) => {
                            let change = &model.changes[index];
                            if !filtered {
                                draw_lines(list, &s, row, graph, index);
                            }
                            draw_dot(list, &s, theme, row, graph, index, change);
                            let caret = if expanded.contains(&change.id) {
                                PhosphorIcon::CaretDown
                            } else {
                                PhosphorIcon::CaretRight
                            };
                            if change.revision_count > 0 {
                                list.phosphor_icon(Rect::new(columns.title - 14.0, row.y + 6.0, 11.0, 11.0), caret, dim);
                            }
                            let short_w = chrome::mono(list, &s, Rect::new(columns.title, row.y, 50.0, row.height), &change.short, TextSize::Meta, dim);
                            let title_x = columns.title + short_w + 8.0;
                            let tag_w = if model.workspace.selected == Some(index) {
                                Badge::new(BadgeTone::Live).draw_right(list, &s, columns.author - 8.0, row.y + (row.height - Badge::new(BadgeTone::Live).height()) * 0.5, "selected").width + 6.0
                            } else {
                                0.0
                            };
                            chrome::sans(list, &s, Rect::new(title_x, row.y, (columns.author - 8.0 - tag_w - title_x).max(0.0), row.height), &change.title, TextSize::Row, ink);
                            let icon = if change.agent { PhosphorIcon::Robot } else { PhosphorIcon::User };
                            list.phosphor_icon(Rect::new(columns.author, row.y + 6.0, 12.0, 12.0), icon, dim);
                            chrome::sans(list, &s, Rect::new(columns.author + 16.0, row.y, AUTHOR_W - 20.0, row.height), &change.author, TextSize::Row, ink);
                            chrome::mono(list, &s, Rect::new(columns.revs, row.y, REVS_W, row.height), &change.revision_count.to_string(), TextSize::Meta, dim);
                            let (tone, label) = state_badge(change);
                            let badge = Badge::new(tone);
                            badge.draw(list, &s, columns.state, row.y + (row.height - badge.height()) * 0.5, label);
                            chrome::mono(list, &s, Rect::new(columns.age, row.y, AGE_W, row.height), &change.age, TextSize::Meta, dim);
                        }
                        Row::Revision(index, revision) => {
                            let change = &model.changes[index];
                            if !filtered {
                                let line = s.ink(Ink::Dim);
                                for lane in graph.rows[index].below() {
                                    let x = lane_x(row, lane);
                                    list.line([x, row.y], [x, row.bottom()], 1.0, line);
                                }
                            }
                            let Some(rev) = change.revision(revision) else { return };
                            let w = chrome::mono(list, &s, Rect::new(columns.title + 12.0, row.y, 40.0, row.height), &format!("r{}", revision + 1), TextSize::Meta, dim);
                            chrome::sans(list, &s, Rect::new(columns.title + 20.0 + w, row.y, (columns.author - 28.0 - w - columns.title).max(0.0), row.height), &rev.message, TextSize::Row, ink);
                            chrome::sans(list, &s, Rect::new(columns.author + 16.0, row.y, AUTHOR_W - 20.0, row.height), &rev.actor, TextSize::Row, dim);
                            chrome::mono(list, &s, Rect::new(columns.state, row.y, STATE_W, row.height), if change.published { "pub" } else { "local" }, TextSize::Meta, dim);
                            chrome::mono(list, &s, Rect::new(columns.age, row.y, AGE_W, row.height), &rev.age, TextSize::Meta, dim);
                        }
                        Row::Older => {
                            let text = format!("{} older published changes · list virtualises · type to search all", group_digits(older));
                            chrome::sans(list, &s, Rect::new(columns.title, row.y, row.right() - columns.title - 8.0, row.height), &text, TextSize::Row, s.ink(Ink::Caption));
                        }
                    }
                },
            );
        if let Some(clicked) = output.clicked {
            match self.rows[clicked] {
                Row::Change(index) => {
                    let id = model.changes[index].id.clone();
                    let on_caret = frame.input.mouse_x < Columns::new(body).title - 2.0
                        && frame.input.mouse_x > Columns::new(body).title - 18.0;
                    let was_picked = self.picked.as_deref() == Some(id.as_str());
                    if on_caret || was_picked {
                        if !self.expanded.remove(&id) {
                            self.expanded.insert(id);
                            self.fetch_revisions(model, pending);
                        }
                        self.layout_rows(model);
                    }
                    self.pick(model, index, None, pending);
                }
                Row::Revision(index, revision) => self.pick(model, index, Some(revision), pending),
                Row::Older => {}
            }
        }
    }

    fn draw_detail(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        model: &Model,
        pending: &mut Vec<Request>,
    ) {
        let theme = frame.theme;
        let s = StyleResolver::new(theme);
        chrome::pane(frame.list, theme, rect);
        let Some((index, revision)) = self
            .picked_revision(model)
            .or_else(|| self.picked_index(model).map(|index| (index, 0)))
        else {
            let text = "Pick a change to see its revisions.";
            chrome::sans(
                frame.list,
                &s,
                Rect::new(rect.x + 16.0, rect.y + 16.0, rect.width - 32.0, 18.0),
                text,
                TextSize::Menu,
                s.ink(Ink::Caption),
            );
            return;
        };
        let change = &model.changes[index];
        let x = rect.x + 16.0;
        let width = rect.width - 32.0;
        let mut y = rect.y + 14.0;
        let w = chrome::mono(
            frame.list,
            &s,
            Rect::new(x, y, 80.0, 16.0),
            &change.short,
            TextSize::Row,
            s.ink(Ink::Emph),
        );
        let mut badge_x = x + w + 8.0;
        let badge_y = y + (16.0 - Badge::new(BadgeTone::Draft).height()) * 0.5;
        if change.conflicted {
            badge_x = Badge::new(BadgeTone::Error)
                .draw(frame.list, &s, badge_x, badge_y, "conflict")
                .right()
                + 6.0;
        }
        let (tone, label) = if change.published {
            (BadgeTone::Baked, "published")
        } else {
            (BadgeTone::Draft, "local")
        };
        Badge::new(tone).draw(frame.list, &s, badge_x, badge_y, label);
        y += 22.0;
        chrome::sans(
            frame.list,
            &s,
            Rect::new(x, y, width, 22.0),
            &change.title,
            TextSize::Menu,
            s.ink(Ink::Title),
        );
        y += 32.0;
        let shorts = |indexes: &[usize]| -> String {
            indexes
                .iter()
                .filter_map(|&at| model.changes.get(at))
                .map(|change| change.short.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let built_on = if change.independent {
            "nothing · independent".to_owned()
        } else if change.depends_on.is_empty() {
            "the frontier".to_owned()
        } else {
            shorts(&change.depends_on)
        };
        let dependents = model.dependents(index);
        let dependents = if dependents.is_empty() {
            "none".to_owned()
        } else {
            shorts(&dependents)
        };
        let revisions = format!(
            "{} · {}",
            change.revision_count,
            if change.published {
                "published"
            } else {
                "unpublished"
            }
        );
        let author = format!(
            "{}{}",
            change.author,
            if change.agent { " · agent" } else { "" }
        );
        let facts = [
            ("Author", author),
            ("Built on", built_on),
            ("Dependents", dependents),
            ("Revisions", revisions),
        ];
        for (key, value) in &facts {
            chrome::caps(
                frame.list,
                &s,
                Rect::new(x, y, 90.0, 18.0),
                key,
                s.ink(Ink::Label),
            );
            chrome::mono(
                frame.list,
                &s,
                Rect::new(x + 96.0, y, width - 96.0, 18.0),
                value,
                TextSize::Meta,
                s.ink(Ink::Body2),
            );
            y += 20.0;
        }
        y += 10.0;
        let mut bx = x;
        let is_selected = model.workspace.selected == Some(index);
        let buttons: [(&str, Tone, bool, Option<Request>); 5] = [
            (
                "Select as Active",
                Tone::Accent,
                !is_selected && !change.published,
                Some(Request::SelectChange {
                    change: change.id.clone(),
                }),
            ),
            (
                "Squash Change",
                Tone::Default,
                change.revision_count > 1 && !change.published,
                Some(Request::SquashChange {
                    change: change.id.clone(),
                }),
            ),
            (
                "Squash Stack",
                Tone::Default,
                !change.published && !change.depends_on.is_empty(),
                Some(Request::SquashStack {
                    change: change.id.clone(),
                }),
            ),
            ("Push to Git…", Tone::Default, false, None),
            ("Resolve…", Tone::Danger, false, None),
        ];
        for (label, tone, enabled, request) in buttons {
            if label == "Resolve…" && !change.conflicted {
                continue;
            }
            let button = Button::new(label).tone(tone).enabled(enabled);
            let w = button.intrinsic_size(frame.list, &s).0;
            if button.draw(Rect::new(bx, y, w, 24.0), &mut frame.cx())
                && let Some(request) = request
            {
                pending.push(request);
            }
            bx += w + 8.0;
        }
        y += 36.0;
        let head = Rect::new(rect.x, y, rect.width, HEADER_H);
        chrome::hrule(frame.list, theme, rect.x, y - 1.0, rect.width);
        frame.list.paint_quad(head, theme.chrome.dock.header);
        if let Some(rev) = change.revision(revision) {
            let w = chrome::mono(
                frame.list,
                &s,
                Rect::new(head.x + 9.0, head.y, 40.0, head.height),
                &format!("r{}", revision + 1),
                TextSize::Meta,
                s.ink(Ink::Emph),
            );
            chrome::sans(
                frame.list,
                &s,
                Rect::new(
                    head.x + 17.0 + w,
                    head.y,
                    head.width - 140.0 - w,
                    head.height,
                ),
                &rev.message,
                TextSize::Row,
                s.ink(Ink::Title),
            );
            let aside = if revision == 0 {
                "vs its base"
            } else {
                "vs previous"
            };
            chrome::mono_right(
                frame.list,
                &s,
                Rect::new(head.x, head.y, head.width - 9.0, head.height),
                aside,
                TextSize::Meta,
                s.ink(Ink::Dim),
            );
        }
        let well = Rect::new(
            rect.x,
            head.bottom(),
            rect.width,
            (rect.bottom() - head.bottom()).max(0.0),
        );
        diff_view::draw(
            frame,
            well,
            &self.diff,
            &mut self.diff_scroll,
            PhosphorIcon::File,
        );
    }
}

/// Column positions inside a row of the change list.
struct Columns {
    title: f32,
    author: f32,
    revs: f32,
    state: f32,
    age: f32,
}

impl Columns {
    fn new(row: Rect) -> Self {
        let age = row.right() - 8.0 - AGE_W;
        let state = age - STATE_W;
        let revs = state - REVS_W;
        let author = revs - AUTHOR_W;
        Self {
            title: row.x + GRAPH_W + 14.0,
            author,
            revs,
            state,
            age,
        }
    }
}

fn matches(change: &crate::model::Change, needle: &str) -> bool {
    change.title.to_lowercase().contains(needle)
        || change.author.to_lowercase().contains(needle)
        || change.short.starts_with(needle)
        || change.id.starts_with(needle)
}

fn state_badge(change: &crate::model::Change) -> (BadgeTone, &'static str) {
    if change.conflicted {
        (BadgeTone::Error, "conflict")
    } else if change.published {
        (BadgeTone::Baked, "pub")
    } else {
        (BadgeTone::Draft, "local")
    }
}

fn lane_x(row: Rect, lane: u16) -> f32 {
    row.x + LANE_X + 3.5 + f32::from(lane.min(4)) * LANE_GAP
}

fn draw_lines(
    list: &mut wgpu_gameui::DrawList,
    s: &StyleResolver,
    row: Rect,
    graph: &Graph,
    index: usize,
) {
    let line = s.ink(Ink::Dim);
    let at = &graph.rows[index];
    let node = lane_x(row, at.lane);
    let mid = row.y + row.height * 0.5;
    for &lane in &at.through {
        let x = lane_x(row, lane);
        list.line([x, row.y], [x, row.bottom()], 1.0, line);
    }
    for &lane in &at.from_above {
        list.line([lane_x(row, lane), row.y], [node, mid], 1.0, line);
    }
    for &lane in &at.to_below {
        list.line([node, mid], [lane_x(row, lane), row.bottom()], 1.0, line);
    }
}

fn draw_dot(
    list: &mut wgpu_gameui::DrawList,
    s: &StyleResolver,
    theme: &wgpu_gameui::Theme,
    row: Rect,
    graph: &Graph,
    index: usize,
    change: &crate::model::Change,
) {
    let center = (
        lane_x(row, graph.rows[index].lane),
        row.y + row.height * 0.5,
    );
    if change.conflicted {
        list.circle(center, 3.5, theme.danger_text);
    } else if change.published {
        list.circle(center, 3.5, theme.background);
        list.circle_outline(center, 3.5, 1.0, s.ink(Ink::Dim));
    } else {
        list.circle(center, 3.5, s.ink(Ink::Body2));
    }
    if change.independent {
        list.line(
            [center.0 - 4.0, center.1 + 7.0],
            [center.0 + 4.0, center.1 + 7.0],
            1.0,
            s.ink(Ink::Dim),
        );
    }
}
