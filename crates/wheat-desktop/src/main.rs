//! The Wheat desktop client: a window onto one Wheat workspace and the store it
//! shares with the other workspaces, drawn with wgpu-gameui. The design is the
//! Claude Design comp `Wheat Client.dc.html` (see the repository's CLAUDE.md).

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod backend;
mod chrome;
mod diff_view;
mod fixture;
mod graph;
mod history_screen;
mod live;
mod model;
mod sidebar;
mod snapshot;
mod workspace_screen;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use wgpu_gameui::{LayerStack, UiRenderer};
use wheat_client::authoring_metadata::UserConfig;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use app::App;

/// Off the window: nothing in it is under the pointer.
const POINTER_AWAY: f32 = -10_000.0;

struct Gpu {
    ui: UiRenderer,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

struct Desktop {
    app: App,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    layers: Option<LayerStack>,
    modifiers: ModifiersState,
    /// When the next frame is due without input: a toast's age, a caret.
    repaint_at: Option<Instant>,
}

impl ApplicationHandler for Desktop {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Wheat")
                        .with_inner_size(winit::dpi::LogicalSize::new(1600.0, 1000.0)),
                )
                .expect("create window"),
        );
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("request adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("wheat desktop device"),
                ..Default::default()
            },
            None,
        ))
        .expect("request device");
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        let ui = UiRenderer::new(&device, &queue, format, self.app.fonts());
        self.gpu = Some(Gpu {
            ui,
            surface,
            config,
            device,
            queue,
        });
        window.request_redraw();
        self.window = Some(window);
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let input = &mut self.app.input;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    gpu.ui.resize(
                        &gpu.device,
                        &gpu.queue,
                        gpu.config.width,
                        gpu.config.height,
                        window.scale_factor() as f32,
                    );
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = window.scale_factor();
                input.mouse_x = (position.x / scale) as f32;
                input.mouse_y = (position.y / scale) as f32;
            }
            WindowEvent::CursorLeft { .. } if !input.mouse_down => {
                input.mouse_x = POINTER_AWAY;
                input.mouse_y = POINTER_AWAY;
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let down = state == ElementState::Pressed;
                input.mouse_clicked = down && !input.mouse_down;
                input.mouse_released = !down && input.mouse_down;
                input.mouse_down = down;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let [x, y] = match delta {
                    MouseScrollDelta::LineDelta(x, y) => [x, y],
                    MouseScrollDelta::PixelDelta(p) => {
                        let scale = window.scale_factor() * 20.0;
                        [(p.x / scale) as f32, (p.y / scale) as f32]
                    }
                };
                input.scroll_delta += y;
                input.scroll_delta_x += x;
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                input.ctrl_pressed = if cfg!(target_os = "macos") {
                    self.modifiers.super_key()
                } else {
                    self.modifiers.control_key()
                };
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::Enter if down => input.enter_pressed = true,
                        KeyCode::Backspace if down => input.backspace_pressed = true,
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => input.shift_pressed = down,
                        KeyCode::ArrowLeft => input.key_left = down,
                        KeyCode::ArrowRight => input.key_right = down,
                        KeyCode::ArrowUp => input.key_up = down,
                        KeyCode::ArrowDown => input.key_down = down,
                        KeyCode::Escape if down => input.key_escape = true,
                        KeyCode::Tab if down => input.key_tab = true,
                        KeyCode::Space if down => input.key_space = true,
                        KeyCode::Home if down => input.key_home = true,
                        KeyCode::End if down => input.key_end = true,
                        KeyCode::Delete if down => input.key_delete = true,
                        KeyCode::KeyA if down && input.ctrl_pressed => input.key_select_all = true,
                        KeyCode::KeyC if down && input.ctrl_pressed => input.key_copy = true,
                        KeyCode::KeyX if down && input.ctrl_pressed => input.key_cut = true,
                        KeyCode::KeyV if down && input.ctrl_pressed => input.key_paste = true,
                        _ => {}
                    }
                    if down && input.ctrl_pressed {
                        self.app.shortcut(code);
                    }
                }
                let input = &mut self.app.input;
                if down && !input.ctrl_pressed {
                    for text in event.text.iter() {
                        input
                            .text_input
                            .extend(text.chars().filter(|character| !character.is_control()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.draw(&window);
                if self.app.quitting {
                    event_loop.exit();
                }
                return;
            }
            _ => {}
        }
        window.request_redraw();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = &self.window else {
            return;
        };
        if self.app.drain_updates() {
            window.request_redraw();
        }
        match self.repaint_at {
            Some(at) if Instant::now() >= at => {
                self.repaint_at = None;
                window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, (): ()) {
        if self.app.drain_updates()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

impl Desktop {
    fn draw(&mut self, window: &Window) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let frame = match gpu.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(_) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                window.request_redraw();
                return;
            }
        };
        let scale = window.scale_factor() as f32;
        let (width, height) = (
            gpu.config.width as f32 / scale,
            gpu.config.height as f32 / scale,
        );
        let mut layers = self.layers.take().unwrap_or_default();
        layers.clear();
        self.app.build_ui(&mut layers, width, height);
        self.repaint_at = layers
            .next_repaint()
            .map(|seconds| Instant::now() + std::time::Duration::from_secs_f32(seconds));
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("wheat desktop encoder"),
            });
        gpu.ui.begin_frame();
        {
            let clear = gpu.ui.clear_color(self.app.theme.background).color();
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        gpu.ui.render_layers(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &view,
            (gpu.config.width, gpu.config.height),
            scale,
            &layers,
        );
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        self.layers = Some(layers);
        self.app.input.end_frame();
        self.app.after_frame();
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: wheat-desktop [--fixture | --workspace DIR]\n       \
         wheat-desktop --render-ui PATH.png [--screen workspace|changes] [--fixture | --workspace DIR]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut render_to = None;
    let mut screen = app::Screen::Workspace;
    // `None` shows the comp's demo data.
    let mut workspace = Some(std::env::current_dir().unwrap_or_default());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--render-ui" => match args.next() {
                Some(path) => render_to = Some(PathBuf::from(path)),
                None => return usage(),
            },
            "--screen" => match args.next().as_deref().and_then(app::Screen::parse) {
                Some(parsed) => screen = parsed,
                None => return usage(),
            },
            "--fixture" => workspace = None,
            "--workspace" => match args.next() {
                Some(path) => workspace = Some(PathBuf::from(path)),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let source = match workspace {
        None => app::Source::Fixture,
        Some(path) => match UserConfig::from_environment() {
            Ok(user) => app::Source::Workspace { path, user },
            Err(error) => {
                eprintln!("wheat-desktop: {error:#}");
                return ExitCode::FAILURE;
            }
        },
    };
    if let Some(path) = render_to {
        return match snapshot::capture(&path, source, screen) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("wheat-desktop: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let event_loop = match EventLoop::<()>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("wheat-desktop: {error}");
            return ExitCode::FAILURE;
        }
    };
    let proxy = event_loop.create_proxy();
    let wake: backend::Wake = Arc::new(move || {
        let _ = proxy.send_event(());
    });
    let mut app = App::new(source, wake);
    app.screen = screen;
    let mut handler = Desktop {
        app,
        window: None,
        gpu: None,
        layers: None,
        modifiers: ModifiersState::default(),
        repaint_at: None,
    };
    match event_loop.run_app(&mut handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wheat-desktop: {error}");
            ExitCode::FAILURE
        }
    }
}
