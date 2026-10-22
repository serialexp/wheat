//! The window's state and its frame: menu bar, workspaces sidebar, screen rail,
//! the screen itself, and the status bar.

use std::path::PathBuf;
use std::time::Instant;

use wgpu_gameui::layout::Rect;
use wgpu_gameui::{
    AppShell, CountBubble, DrawContext, DrawList, FocusState, FontSystemHandle, IconKey,
    InputState, LayerStack, ManualNav, Menu, MenuBar, MenuBarState, MenuDrawEnv, MenuItem,
    PhosphorIcon, Status, StatusPart, StatusZone, StyleResolver, Theme, Toast, Tone, UiState,
    ZonedStatusBar,
};
use winit::keyboard::KeyCode;

use crate::backend::{Backend, Client, FixtureBackend, Request, Update, Wake};
use crate::chrome;
use crate::history_screen::HistoryScreen;
use crate::live::LiveBackend;
use crate::model::{Model, WorkspaceState};
use crate::sidebar::Sidebar;
use crate::workspace_screen::WorkspaceScreen;

pub const SIDEBAR_W: f32 = 248.0;
pub const RAIL_W: f32 = 44.0;
const MENU_ID: u64 = 0x5748_0001;

/// Where the window's data comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// The comp's demo repository.
    Fixture,
    /// A live workspace directory.
    Workspace(PathBuf),
}

fn backend(source: Source) -> Box<dyn Backend> {
    match source {
        Source::Fixture => Box::new(FixtureBackend::new()),
        Source::Workspace(path) => Box::new(LiveBackend::new(path)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Workspace,
    Changes,
    Labels,
    Conflicts,
    Workspaces,
    Remotes,
    Maintenance,
}

impl Screen {
    pub const ALL: [Screen; 7] = [
        Screen::Workspace,
        Screen::Changes,
        Screen::Labels,
        Screen::Conflicts,
        Screen::Workspaces,
        Screen::Remotes,
        Screen::Maintenance,
    ];

    pub fn parse(name: &str) -> Option<Screen> {
        match name {
            "workspace" => Some(Screen::Workspace),
            "changes" => Some(Screen::Changes),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Screen::Workspace => "Workspace",
            Screen::Changes => "Changes & revisions",
            Screen::Labels => "Labels, views, frontiers",
            Screen::Conflicts => "Conflicts",
            Screen::Workspaces => "Workspaces & agents",
            Screen::Remotes => "Remotes & sync",
            Screen::Maintenance => "Maintenance",
        }
    }

    fn icon(self) -> PhosphorIcon {
        match self {
            Screen::Workspace => PhosphorIcon::Files,
            Screen::Changes => PhosphorIcon::Stack,
            Screen::Labels => PhosphorIcon::Tag,
            Screen::Conflicts => PhosphorIcon::WarningDiamond,
            Screen::Workspaces => PhosphorIcon::Robot,
            Screen::Remotes => PhosphorIcon::CloudArrowUp,
            Screen::Maintenance => PhosphorIcon::Wrench,
        }
    }

    /// The first build has the Workspace and Changes screens; the rest come
    /// later.
    pub fn built(self) -> bool {
        matches!(self, Screen::Workspace | Screen::Changes)
    }
}

/// What a screen draws into for one frame.
pub struct Frame<'a> {
    pub list: &'a mut DrawList,
    pub focus: &'a mut FocusState,
    pub theme: &'a Theme,
    pub input: &'a mut InputState,
    pub width: f32,
    pub height: f32,
}

impl Frame<'_> {
    pub fn cx(&mut self) -> DrawContext<'_> {
        DrawContext::new(
            self.list,
            self.focus,
            self.theme,
            self.input,
            self.width,
            self.height,
        )
    }
}

pub struct App {
    pub theme: Theme,
    pub input: InputState,
    pub ui_state: UiState,
    pub screen: Screen,
    pub quitting: bool,
    fonts: Option<FontSystemHandle>,
    menu: MenuBarState,
    client: Client,
    pub model: Option<Model>,
    load_error: Option<String>,
    pub workspace: WorkspaceScreen,
    pub history: HistoryScreen,
    sidebar: Sidebar,
    /// The status bar's "last:" line.
    last: String,
    last_tick: Instant,
    /// Requests a frame made, sent once it is drawn.
    pending: Vec<Request>,
}

impl App {
    pub fn new(source: Source, wake: Wake) -> Self {
        Self::with_client(Client::threaded(backend(source), wake))
    }

    /// An app whose backend answers inline: for screenshots and tests.
    pub fn inline(source: Source) -> Self {
        let mut app = Self::with_client(Client::inline(backend(source)));
        app.drain_updates();
        app
    }

    fn with_client(mut client: Client) -> Self {
        client.send(Request::Load);
        Self {
            theme: Theme::default(),
            input: InputState::default(),
            ui_state: UiState::default(),
            screen: Screen::Workspace,
            quitting: false,
            fonts: None,
            menu: MenuBarState::new(),
            client,
            model: None,
            load_error: None,
            workspace: WorkspaceScreen::default(),
            history: HistoryScreen::default(),
            sidebar: Sidebar::default(),
            last: "open".into(),
            last_tick: Instant::now(),
            pending: Vec::new(),
        }
    }

    pub fn fonts(&mut self) -> FontSystemHandle {
        self.fonts
            .get_or_insert_with(wgpu_gameui::shared_font_system)
            .clone()
    }

    /// Takes in what the backend has sent; true when there was any.
    pub fn drain_updates(&mut self) -> bool {
        let mut any = false;
        while let Some(update) = self.client.try_recv() {
            any = true;
            match update {
                Update::Loaded(Ok(model)) => {
                    self.workspace.loaded(&model, &mut self.pending);
                    self.history.loaded(&model, &mut self.pending);
                    self.model = Some(model);
                    self.load_error = None;
                }
                Update::Loaded(Err(error)) => self.load_error = Some(error),
                Update::WorkspaceDiff { path, diff } => self.workspace.diff_arrived(path, diff),
                Update::RevisionDiff {
                    change,
                    revision,
                    diff,
                } => self.history.diff_arrived(change, revision, diff),
                Update::Revisions {
                    change,
                    revisions,
                    complete,
                } => match revisions {
                    Ok(revisions) => {
                        if let Some(model) = self.model.as_mut()
                            && let Some(at) = model.changes.iter().position(|c| c.id == change)
                        {
                            let target = &mut model.changes[at];
                            // A change's count is an estimate until its whole chain is read.
                            target.revision_count = if complete {
                                revisions.len()
                            } else {
                                target.revision_count.max(revisions.len())
                            };
                            target.revisions = revisions;
                            self.history.revisions_arrived(model, &mut self.pending);
                        }
                    }
                    Err(error) => {
                        self.ui_state
                            .toasts
                            .push(Toast::error(error).with_title("revisions"));
                    }
                },
                Update::Done { action, outcome } => {
                    self.last = action;
                    match outcome {
                        Ok((title, message)) => {
                            self.ui_state
                                .toasts
                                .push(Toast::success(message).with_title(title));
                        }
                        Err(error) => {
                            self.ui_state
                                .toasts
                                .push(Toast::error(error).with_title(self.last.clone()));
                        }
                    }
                }
            }
        }
        self.flush();
        any
    }

    /// After a frame: send what it asked for.
    pub fn after_frame(&mut self) {
        self.flush();
    }

    fn flush(&mut self) {
        for request in std::mem::take(&mut self.pending) {
            if !matches!(
                request,
                Request::Load
                    | Request::WorkspaceDiff { .. }
                    | Request::RevisionDiff { .. }
                    | Request::Revisions { .. }
            ) {
                self.last = crate::backend::action_name(&request);
            }
            self.client.send(request);
        }
    }

    pub fn shortcut(&mut self, key: KeyCode) {
        let digit = match key {
            KeyCode::Digit1 => Some(0),
            KeyCode::Digit2 => Some(1),
            KeyCode::Digit3 => Some(2),
            KeyCode::Digit4 => Some(3),
            KeyCode::Digit5 => Some(4),
            KeyCode::Digit6 => Some(5),
            KeyCode::Digit7 => Some(6),
            _ => None,
        };
        if let Some(screen) = digit.map(|index| Screen::ALL[index])
            && screen.built()
        {
            self.screen = screen;
        }
        if key == KeyCode::KeyQ {
            self.quitting = true;
        }
        if key == KeyCode::Enter
            && self.screen == Screen::Workspace
            && let Some(model) = &self.model
        {
            self.workspace.add(model, &mut self.pending);
        }
    }

    fn menu_items(&self) -> MenuItems {
        let later = "Not built yet";
        let view: Vec<MenuItem<'static>> = Screen::ALL
            .iter()
            .enumerate()
            .map(|(index, screen)| {
                MenuItem::new(screen.title())
                    .id(VIEW_BASE + index as u64)
                    .shortcut(SCREEN_KEYS[index])
                    .checked(*screen == self.screen)
                    .enabled(screen.built())
                    .reason(later)
            })
            .collect();
        MenuItems {
            repository: vec![
                MenuItem::new("Open…")
                    .shortcut("Ctrl O")
                    .enabled(false)
                    .reason(later),
                MenuItem::new("Import from Git…")
                    .enabled(false)
                    .reason(later),
                MenuItem::new("Clone from Wheat Server…")
                    .enabled(false)
                    .reason(later),
                MenuItem::separator(),
                MenuItem::new("Reload").id(RELOAD),
                MenuItem::separator(),
                MenuItem::new("Quit").id(QUIT).shortcut("Ctrl Q"),
            ],
            change: vec![
                MenuItem::new("Add Picked Paths")
                    .id(ADD)
                    .shortcut("Ctrl Enter"),
                MenuItem::new("Squash Change").id(SQUASH),
                MenuItem::separator(),
                MenuItem::new("Restore Paths").id(RESTORE).danger(true),
            ],
            view,
            remote: vec![
                MenuItem::new("Pull").enabled(false).reason(later),
                MenuItem::new("Push…").enabled(false).reason(later),
                MenuItem::separator(),
                MenuItem::new("Fetch").enabled(false).reason(later),
                MenuItem::new("Publish").enabled(false).reason(later),
            ],
        }
    }

    pub fn build_ui(&mut self, layers: &mut LayerStack, width: f32, height: f32) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.ui_state.set_screen(width, height);
        self.ui_state
            .begin_frame(&mut self.input, &self.theme, dt, &ManualNav);
        self.ui_state.tooltips.clear();
        self.menu.begin_frame(&mut self.input);
        let styles = StyleResolver::new(&self.theme);
        let layout = Regions::new(&styles, width, height);
        let slots = self.menu.push_open_layers(layers);
        let toast_area = layout.body;
        let toast_layer = self.ui_state.toasts.push_layer(layers, toast_area, &styles);
        let items = self.menu_items();
        let menus = items.menus();
        let mut base_input = layers.input_for_base(&self.input);
        let mut clicked_screen = None;
        {
            let list = layers.base_mut();
            list.quad(0.0, 0.0, width, height, self.theme.background);
            let mut frame = Frame {
                list,
                focus: &mut self.ui_state.focus,
                theme: &self.theme,
                input: &mut base_input,
                width,
                height,
            };
            if let Some(rect) = layout.menu_bar {
                MenuBar::new(MENU_ID, &menus).draw(rect, &mut self.menu, &mut frame.cx());
                draw_title(&mut frame, rect, self.model.as_ref());
            }
            let Regions {
                body,
                sidebar,
                rail,
                main,
                ..
            } = layout;
            match &self.model {
                Some(model) => {
                    self.sidebar.draw(&mut frame, sidebar, model);
                    clicked_screen = draw_rail(&mut frame, rail, self.screen, model);
                    match self.screen {
                        Screen::Changes => {
                            self.history
                                .draw(&mut frame, main, model, &mut self.pending)
                        }
                        _ => self
                            .workspace
                            .draw(&mut frame, main, model, &mut self.pending),
                    }
                }
                None => {
                    chrome::pane(frame.list, frame.theme, body);
                    let s = StyleResolver::new(frame.theme);
                    let text = self
                        .load_error
                        .as_deref()
                        .unwrap_or("Opening the workspace…");
                    chrome::sans(
                        frame.list,
                        &s,
                        Rect::new(body.x + 24.0, body.y + 24.0, body.width - 48.0, 20.0),
                        text,
                        wgpu_gameui::TextSize::Menu,
                        s.ink(wgpu_gameui::Ink::Caption),
                    );
                }
            }
            if let Some(rect) = layout.status_bar {
                draw_status(
                    self.model.as_ref(),
                    &self.last,
                    frame.list,
                    &styles,
                    frame.input,
                    rect,
                );
            }
        }
        if let Some(screen) = clicked_screen {
            self.screen = screen;
        }
        let activation = {
            let mut env = MenuDrawEnv {
                theme: &self.theme,
                style: None,
                input: &self.input,
                focus: &mut self.ui_state.focus,
                interactions: &mut self.ui_state.interactions,
                animations: Some(&mut self.ui_state.anim),
                cursor: None,
                screen_width: width,
                screen_height: height,
            };
            self.menu.draw_open_layers(layers, slots, &menus, &mut env)
        };
        if let Some(index) = toast_layer {
            self.ui_state.block_under_layer(layers, index);
            let input = layers.input_for_layer(index, &self.input);
            let mut cx = DrawContext::new(
                &mut layers.layers_mut()[index].list,
                &mut self.ui_state.focus,
                &self.theme,
                &input,
                width,
                height,
            )
            .with_layer(index);
            self.ui_state.toasts.draw(toast_area, &mut cx);
        }
        self.ui_state
            .tooltips
            .draw_into_layers(layers, &self.input, &styles, width, height);
        self.menu.end_frame(&mut self.ui_state.focus);
        self.ui_state.end_frame();
        if let Some(item) = activation {
            self.activate(item.id);
        }
    }

    fn activate(&mut self, id: u64) {
        match id {
            QUIT => self.quitting = true,
            RELOAD => self.pending.push(Request::Load),
            ADD => {
                if let Some(model) = &self.model {
                    self.workspace.add(model, &mut self.pending);
                }
            }
            RESTORE => self.workspace.restore(&mut self.pending),
            SQUASH => {
                if let Some(model) = &self.model {
                    self.history.squash_change(model, &mut self.pending);
                }
            }
            id if (VIEW_BASE..VIEW_BASE + Screen::ALL.len() as u64).contains(&id) => {
                self.screen = Screen::ALL[(id - VIEW_BASE) as usize];
            }
            _ => {}
        }
    }
}

fn draw_status(
    model: Option<&Model>,
    last: &str,
    list: &mut DrawList,
    styles: &StyleResolver,
    input: &InputState,
    rect: Rect,
) {
    let Some(model) = model else {
        ZonedStatusBar::new(&[], &[], &[]).draw(rect, list, styles, input);
        return;
    };
    let frontier = format!(
        "{} / {}",
        model.workspace.label,
        model.workspace.frontier.as_deref().unwrap_or("no frontier")
    );
    let change = model
        .workspace
        .selected
        .and_then(|index| model.changes.get(index))
        .map_or_else(
            || "no change selected".to_owned(),
            |change| format!("{} · r{}", change.short, change.revision_count),
        );
    let conflicts = match model.store.conflicted_changes {
        0 => "no conflicts".to_owned(),
        count => format!("{count} conflicted"),
    };
    let running = model
        .workspaces
        .iter()
        .filter(|workspace| workspace.state == WorkspaceState::Running)
        .count();
    let agents = format!("{running} agents running");
    let last = format!("last: {last}");
    let name = [StatusPart::Text(&model.name)];
    let frontier = [StatusPart::Text(&frontier)];
    let change = [StatusPart::Text(&change)];
    let conflicts = [StatusPart::Text(&conflicts)];
    let agents = [StatusPart::Dot(Status::Running), StatusPart::Text(&agents)];
    let last = [StatusPart::Text(&last)];
    let left = [
        StatusZone::new(&name),
        StatusZone::new(&frontier),
        StatusZone::new(&change),
        StatusZone::new(&conflicts),
        StatusZone::new(&agents),
    ];
    let right = [StatusZone::new(&last)];
    ZonedStatusBar::new(&[], &left, &right).draw(rect, list, styles, input);
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

/// Where the window's parts go.
#[derive(Clone, Copy, Debug)]
pub struct Regions {
    pub menu_bar: Option<Rect>,
    /// Everything between the menu bar and the status bar.
    pub body: Rect,
    pub sidebar: Rect,
    pub rail: Rect,
    /// The current screen's.
    pub main: Rect,
    pub status_bar: Option<Rect>,
}

impl Regions {
    pub fn new(styles: &StyleResolver, width: f32, height: f32) -> Self {
        let screen = Rect::new(0.0, 0.0, width, height);
        let layout = AppShell::new()
            .with_menu_bar()
            .with_status_bar()
            .layout(screen, None, None, None, None, styles);
        let body = layout.viewport;
        let sidebar = Rect::new(body.x, body.y, SIDEBAR_W, body.height);
        let rail = Rect::new(sidebar.right(), body.y, RAIL_W, body.height);
        let main = Rect::new(
            rail.right(),
            body.y,
            (body.right() - rail.right()).max(0.0),
            body.height,
        );
        Self {
            menu_bar: layout.menu_bar,
            body,
            sidebar,
            rail,
            main,
            status_bar: layout.status_bar,
        }
    }
}

/// The menu bar's items, which its menus borrow.
struct MenuItems {
    repository: Vec<MenuItem<'static>>,
    change: Vec<MenuItem<'static>>,
    view: Vec<MenuItem<'static>>,
    remote: Vec<MenuItem<'static>>,
}

impl MenuItems {
    fn menus(&self) -> [Menu<'_>; 4] {
        [
            Menu::new("Repository").with_items(&self.repository),
            Menu::new("Change").with_items(&self.change),
            Menu::new("View").with_items(&self.view),
            Menu::new("Remote").with_items(&self.remote),
        ]
    }
}

const QUIT: u64 = 1;
const RELOAD: u64 = 2;
const ADD: u64 = 3;
const RESTORE: u64 = 4;
const SQUASH: u64 = 5;
const VIEW_BASE: u64 = 100;
const SCREEN_KEYS: [&str; 7] = [
    "Ctrl 1", "Ctrl 2", "Ctrl 3", "Ctrl 4", "Ctrl 5", "Ctrl 6", "Ctrl 7",
];

/// The brand and the open repository, over the menu bar's empty middle and
/// right.
fn draw_title(frame: &mut Frame<'_>, rect: Rect, model: Option<&Model>) {
    let s = StyleResolver::new(frame.theme);
    let brand_w = s.mono_width(frame.list, "wheat", wgpu_gameui::TextSize::Row);
    chrome::mono(
        frame.list,
        &s,
        Rect::new(
            rect.right() - 12.0 - brand_w,
            rect.y,
            brand_w + 1.0,
            rect.height,
        ),
        "wheat",
        wgpu_gameui::TextSize::Row,
        s.ink(wgpu_gameui::Ink::Glyph),
    );
    let Some(model) = model else {
        return;
    };
    let document = format!("{} · {}", model.name, model.path);
    frame.list.text(
        s.sans_block(
            &document,
            rect.x,
            rect.y,
            wgpu_gameui::TextSize::Row,
            wgpu_gameui::Ink::Caption,
        )
        .with_line_height(rect.height)
        .with_max_width(rect.width)
        .with_ellipsis()
        .with_align(wgpu_gameui::TextAlign::Center),
    );
}

/// The rail of screen keys between the sidebar and the screen. Returns the
/// screen clicked.
fn draw_rail(frame: &mut Frame<'_>, rect: Rect, current: Screen, model: &Model) -> Option<Screen> {
    let theme = frame.theme;
    let [start, end] = theme.chrome.toolbar.rail_colors;
    frame
        .list
        .linear_gradient(rect, start, end, wgpu_gameui::GradientAxis::Horizontal);
    chrome::vrule(frame.list, theme, rect.right() - 1.0, rect.y, rect.height);
    let size = 30.0;
    let mut clicked = None;
    let mut y = rect.y + 8.0;
    for screen in Screen::ALL {
        let key = Rect::new(rect.x + (rect.width - size) * 0.5, y, size, size);
        let on = screen == current;
        let response = IconKey::new(screen.icon(), size)
            .tone(if on { Tone::Latch } else { Tone::Ghost })
            .held(on)
            .enabled(screen.built())
            .draw(key, &mut frame.cx());
        if response.clicked && screen.built() {
            clicked = Some(screen);
        }
        if screen == Screen::Conflicts && model.store.conflicted_changes > 0 {
            let s = StyleResolver::new(theme);
            CountBubble::new(model.store.conflicted_changes as u32).draw(
                frame.list,
                &s,
                key.right() - 9.0,
                key.y - 4.0,
            );
        }
        y += size + 6.0;
    }
    clicked
}
