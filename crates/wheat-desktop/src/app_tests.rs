//! The window driven the way a person would: frames with the pointer moved,
//! pressed and released, against the fixture answering inline.

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{LayerStack, StyleResolver, shared_font_system};
use winit::keyboard::KeyCode;

use super::{App, Regions, Screen, Source};
use crate::chrome::{HEADER_H, TOOLBAR_H};
use crate::model::{Diff, FileGroup};

const SIZE: (f32, f32) = (1600.0, 1000.0);

struct Window {
    app: App,
}

impl Window {
    fn open(screen: Screen) -> Self {
        let mut app = App::inline(Source::Fixture);
        app.screen = screen;
        let mut window = Self { app };
        window.frame();
        window
    }

    fn frame(&mut self) {
        let mut layers = LayerStack::with_font_system(shared_font_system());
        self.app.build_ui(&mut layers, SIZE.0, SIZE.1);
        self.app.input.end_frame();
        self.app.after_frame();
        self.app.drain_updates();
    }

    fn click(&mut self, x: f32, y: f32) {
        self.app.input.mouse_x = x;
        self.app.input.mouse_y = y;
        self.frame();
        self.app.input.mouse_down = true;
        self.app.input.mouse_clicked = true;
        self.frame();
        self.app.input.mouse_down = false;
        self.app.input.mouse_released = true;
        self.frame();
    }

    fn regions(&self) -> Regions {
        Regions::new(&StyleResolver::new(&self.app.theme), SIZE.0, SIZE.1)
    }

    /// The local-edits list's row `index`: groups' captions count as rows.
    fn file_row(&self, index: usize) -> Rect {
        let main = self.regions().main;
        let top = main.y + TOOLBAR_H + HEADER_H;
        Rect::new(main.x, top + index as f32 * 22.0, 400.0, 22.0)
    }

    fn model(&self) -> &crate::model::Model {
        self.app.model.as_ref().expect("the fixture loads")
    }
}

#[test]
fn the_workspace_opens_on_its_first_edit_with_the_diff() {
    let window = Window::open(Screen::Workspace);
    assert_eq!(
        window.app.workspace.selected_path.as_deref(),
        Some("src/lexer/intern.rs")
    );
    assert!(matches!(window.app.workspace.diff(), Diff::Text { .. }));
}

#[test]
fn clicking_a_file_shows_its_diff() {
    let mut window = Window::open(Screen::Workspace);
    // Row 0 is the "Tracked" caption; row 5 the deleted legacy_ident.rs.
    let row = window.file_row(5);
    window.click(row.x + 200.0, row.y + 11.0);
    assert_eq!(
        window.app.workspace.selected_path.as_deref(),
        Some("src/lexer/legacy_ident.rs")
    );
    assert!(matches!(window.app.workspace.diff(), Diff::Opaque { .. }));
    assert!(
        window.app.workspace.checked.is_empty(),
        "a click on the path only picks the diff"
    );
}

#[test]
fn a_group_caption_is_not_picked() {
    let mut window = Window::open(Screen::Workspace);
    let row = window.file_row(0);
    window.click(row.x + 200.0, row.y + 11.0);
    assert_eq!(
        window.app.workspace.selected_path.as_deref(),
        Some("src/lexer/intern.rs")
    );
}

#[test]
fn ticking_files_and_adding_captures_them_into_the_selected_change() {
    let mut window = Window::open(Screen::Workspace);
    for index in [1, 2] {
        let row = window.file_row(index);
        window.click(row.x + 16.0, row.y + 11.0);
    }
    let checked: Vec<&str> = window
        .app
        .workspace
        .checked
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(checked, ["src/lexer/intern.rs", "src/lexer/mod.rs"]);
    let selected = window
        .model()
        .workspace
        .selected
        .expect("the fixture selects a change");
    let revisions = window.model().changes[selected].revisions.len();
    window.app.shortcut(KeyCode::Enter);
    window.frame();
    let model = window.model();
    assert_eq!(model.changes[selected].revisions.len(), revisions + 1);
    assert!(
        !model
            .workspace
            .files
            .iter()
            .any(|file| file.path == "src/lexer/intern.rs" || file.path == "src/lexer/mod.rs")
    );
    assert!(window.app.workspace.checked.is_empty());
    // The captured file was the picked one: the next edit is shown instead.
    assert_eq!(
        window.app.workspace.selected_path.as_deref(),
        Some("src/lexer/token.rs")
    );
    assert_eq!(window.app.last, "add 2 paths");
}

#[test]
fn restore_drops_only_tracked_edits() {
    let mut window = Window::open(Screen::Workspace);
    window.app.workspace.checked.extend([
        "src/lexer/mod.rs".to_owned(),
        "src/lexer/symbol.rs".to_owned(),
    ]);
    window.app.workspace.restore(&mut window.app.pending);
    window.frame();
    let files = &window.model().workspace.files;
    assert!(!files.iter().any(|file| file.path == "src/lexer/mod.rs"));
    assert!(
        files
            .iter()
            .any(|file| file.path == "src/lexer/symbol.rs" && file.group == FileGroup::Untracked)
    );
}

#[test]
fn the_changes_screen_opens_on_the_selected_change_latest_revision() {
    let window = Window::open(Screen::Changes);
    let model = window.model();
    let selected = model
        .workspace
        .selected
        .expect("the fixture selects a change");
    let change = &model.changes[selected];
    assert_eq!(
        window.app.history.diff_for(),
        Some(&(change.id.clone(), change.revisions.len() - 1))
    );
}

#[test]
fn clicking_a_revision_row_shows_that_revision() {
    let mut window = Window::open(Screen::Changes);
    let selected = window
        .model()
        .workspace
        .selected
        .expect("the fixture selects a change");
    let id = window.model().changes[selected].id.clone();
    let main = window.regions().main;
    // The changes above the selected one, its row, then its newest revision
    // first: the third row under it is r1.
    let top = main.y + TOOLBAR_H + HEADER_H + (selected + 1) as f32 * 24.0;
    window.click(main.x + 300.0, top + 2.0 * 22.0 + 11.0);
    assert_eq!(window.app.history.revision, Some(0));
    assert_eq!(window.app.history.diff_for(), Some(&(id, 0)));
}

#[test]
fn a_live_workspace_says_it_cannot_open_yet() {
    let mut app = App::inline(Source::Workspace("/nowhere".into()));
    let mut layers = LayerStack::with_font_system(shared_font_system());
    app.build_ui(&mut layers, SIZE.0, SIZE.1);
    assert!(app.model.is_none());
    assert!(
        app.load_error
            .as_deref()
            .is_some_and(|error| error.contains("--fixture"))
    );
}
