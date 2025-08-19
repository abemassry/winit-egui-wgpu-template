use anyhow::Context;
use clap;
use futures::executor::block_on;
use wasi_frame_buffer_wasmtime::WasiFrameBufferView;
use wasi_graphics_context_wasmtime::WasiGraphicsContextView;
use wasi_surface_wasmtime::{Surface, SurfaceDesc, WasiSurfaceView, SurfaceProxy, WasiWinitEventLoopProxy};
use wasi_webgpu_wasmtime::WasiWebGpuView;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use wasmtime_wasi::{IoView, ResourceTable};

use std::collections::HashMap;
use crate::egui_tools::EguiRenderer;
use egui_wgpu::wgpu::SurfaceError;
use egui_wgpu::{wgpu, ScreenDescriptor};
use std::sync::{Arc, Mutex};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalSize, Position};
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};
use winit::raw_window_handle::HasRawWindowHandle;
use winit::keyboard::ModifiersState;

use egui_commonmark::*;
use pulldown_cmark::{Parser as PullDownParser, Options};

#[path = "fill.rs"]
mod fill;

#[derive(clap::Parser, Debug)]
struct RuntimeArgs {
    /// The example name
    #[arg(long)]
    example: String,
}

wasmtime::component::bindgen!({
    path: "wasi-gfx-runtime/wit/",
    world: "example",
    async: {
        only_imports: [],
    },
    with: {
        "wasi:graphics-context/graphics-context": wasi_graphics_context_wasmtime::wasi::graphics_context::graphics_context,
        "wasi:surface/surface": wasi_surface_wasmtime::wasi::surface::surface,
        "wasi:frame-buffer/frame-buffer": wasi_frame_buffer_wasmtime::wasi::frame_buffer::frame_buffer,
        "wasi:webgpu/webgpu": wasi_webgpu_wasmtime::wasi::webgpu::webgpu,
    },
});

struct HostState {
    pub table: ResourceTable,
    pub instance: Arc<wgpu_core::global::Global>,
    pub main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy,
}

impl HostState {
    fn new(main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy) -> Self {
        Self {
            table: ResourceTable::new(),
            instance: Arc::new(wgpu_core::global::Global::new(
                "webgpu",
                &wgpu_types::InstanceDescriptor {
                    backends: wgpu_types::Backends::all(),
                    flags: wgpu_types::InstanceFlags::from_build_config(),
                    backend_options: Default::default(),
                },
            )),
            main_thread_proxy,
        }
    }
}

impl IoView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiGraphicsContextView for HostState {}
impl WasiFrameBufferView for HostState {}

struct UiThreadSpawner(wasi_surface_wasmtime::WasiWinitEventLoopProxy);

impl wasi_webgpu_wasmtime::MainThreadSpawner for UiThreadSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        self.0.spawn(f).await
    }
}

impl WasiWebGpuView for HostState {
    fn instance(&self) -> Arc<wgpu_core::global::Global> {
        Arc::clone(&self.instance)
    }

    fn ui_thread_spawner(&self) -> Box<impl wasi_webgpu_wasmtime::MainThreadSpawner + 'static> {
        Box::new(UiThreadSpawner(self.main_thread_proxy.clone()))
    }
}

impl WasiSurfaceView for HostState {
    fn create_canvas(&self, desc: SurfaceDesc) -> Surface {
        block_on(self.main_thread_proxy.create_window(desc))
    }
}

impl ExampleImports for HostState {
    fn print(&mut self, s: String) {
        println!("{s}");
    }
}


#[derive(Clone)]
struct Tab {
    // Example stuff:
    label: String,
    location: String,
    status: String,

    // currently loaded page of tab
    contents: String,

    // for history
    back: Vec<String>,
    forward: Vec<String>,
}

pub struct AppState {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub surface: wgpu::Surface<'static>,
    pub scale_factor: f32,
    pub egui_renderer: EguiRenderer,
}

impl AppState {
    async fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        window: &Window,
        width: u32,
        height: u32,
    ) -> Self {
        let power_pref = wgpu::PowerPreference::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: power_pref,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("Failed to find an appropriate adapter");

        let features = wgpu::Features::empty();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: features,
                    required_limits: Default::default(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .expect("Failed to create device");

        let swapchain_capabilities = surface.get_capabilities(&adapter);
        let selected_format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let swapchain_format = swapchain_capabilities
            .formats
            .iter()
            .find(|d| **d == selected_format)
            .expect("failed to select proper surface texture format!");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: *swapchain_format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 0,
            alpha_mode: swapchain_capabilities.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &surface_config);

        let egui_renderer = EguiRenderer::new(&device, surface_config.format, None, 1, window);

        let scale_factor = 1.0;

        Self {
            device,
            queue,
            surface,
            surface_config,
            egui_renderer,
            scale_factor,
        }
    }

    fn resize_surface(&mut self, width: u32, height: u32) {
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }
}

pub struct App {
    instance: wgpu::Instance,
    state: Option<AppState>,
    window: Option<Arc<Window>>,
    child_window: Option<Window>,
    parent_window_id: WindowId,
    child_window_id: WindowId,
    current_status: String,
    current_location: String,
    current_tab: String,
    current_page: String,
    tabs: Vec<Tab>,

    pointer_pos: HashMap<WindowId, (f64, f64)>,
    modifiers: HashMap<WindowId, ModifiersState>,
    proxies: HashMap<WindowId, SurfaceProxy>,
    arc_proxies: Arc<Mutex<HashMap<WindowId, SurfaceProxy>>>,
}

impl App {
    pub fn new() -> Self {
        let instance = egui_wgpu::wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        Self {
            instance,
            state: None,
            window: None,
            child_window: None,
            parent_window_id: 1.into(),
            child_window_id: 2.into(),
            current_status: "Loading...".to_string(),
            current_location: "https://example.com/".to_string(),
            current_tab: "".to_string(),
            current_page: "".to_string(),
            tabs: vec![Tab {
                label: "".to_string(),
                location: "https://example.com/".to_string(),
                status: "Loaded".to_string(),
                contents: "".to_string(),
                back: Vec::new(),
                forward: Vec::new(),
            }],
            pointer_pos: HashMap::new(),
            modifiers: HashMap::new(),
            proxies: HashMap::new(),
            arc_proxies: Arc::new(Mutex::new(HashMap::new())),
        }
    }



    async fn set_window(&mut self, window: Window) {
        let window = Arc::new(window);
        let initial_width = 1920 * 2;
        let initial_height = 1080 * 2;

        let _ = window.request_inner_size(PhysicalSize::new(initial_width, initial_height));

        let surface = self
            .instance
            .create_surface(window.clone())
            .expect("Failed to create surface!");

        let state = AppState::new(
            &self.instance,
            surface,
            &window,
            initial_width,
            initial_width,
        )
        .await;

        self.window.get_or_insert(window);
        self.state.get_or_insert(state);
    }

    fn handle_resized(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.state.as_mut().unwrap().resize_surface(width, height);
        }
    }

    fn handle_redraw(&mut self) {
        // Attempt to handle minimizing window
        if let Some(window) = self.window.as_ref() {
            if let Some(min) = window.is_minimized() {
                if min {
                    println!("Window is minimized");
                    return;
                }
            }
        }

        let state = self.state.as_mut().unwrap();

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [state.surface_config.width, state.surface_config.height],
            pixels_per_point: self.window.as_ref().unwrap().scale_factor() as f32
                * state.scale_factor,
        };

        let surface_texture = state.surface.get_current_texture();

        match surface_texture {
            Err(SurfaceError::Outdated) => {
                // Ignoring outdated to allow resizing and minimization
                println!("wgpu surface outdated");
                return;
            }
            Err(_) => {
                surface_texture.expect("Failed to acquire next swap chain texture");
                return;
            }
            Ok(_) => {}
        };

        let surface_texture = surface_texture.unwrap();

        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        let window = self.window.as_ref().unwrap();

        {
            state.egui_renderer.begin_frame(window);

            //egui::Window::new("winit + egui + wgpu says hello!")
            //    .resizable(true)
            //    .vscroll(true)
            //    .default_open(false)
            //    .show(state.egui_renderer.context(), |ui| {
            //        ui.label("Label!");

            //        if ui.button("Button!").clicked() {
            //            println!("boom!")
            //        }

            //        ui.separator();
            //        ui.horizontal(|ui| {
            //            ui.label(format!(
            //                "Pixels per point: {}",
            //                state.egui_renderer.context().pixels_per_point()
            //            ));
            //            if ui.button("-").clicked() {
            //                state.scale_factor = (state.scale_factor - 0.1).max(0.3);
            //            }
            //            if ui.button("+").clicked() {
            //                state.scale_factor = (state.scale_factor + 0.1).min(3.0);
            //            }
            //        });
            //    });
            //
            //    egui browser window
            egui::TopBottomPanel::top("top_panel")
                .show(state.egui_renderer.context(), |ui| {
                    egui::menu::bar(ui, |ui| {
                        ui.menu_button(egui_material_icons::icons::ICON_MENU, |ui| {
                            egui::widgets::global_theme_preference_buttons(ui);
                            if ui.button("Quit").clicked() {
                                println!("Quit button clicked");
                            }
                        });
                        ui.add_space(3.0);
                        ui.add_space(1.0);
                        ui.button(egui_material_icons::icons::ICON_ARROW_BACK)
                            .on_hover_text("Back")
                            .clicked().then(|| {
                                if let Err(e) = back() {
                                    self.current_status = e.to_string();
                                } else {
                                    self.current_status = "Loaded".to_string();
                                }
                            });
                        ui.add_space(1.0);
                        ui.button(egui_material_icons::icons::ICON_ARROW_FORWARD)
                            .on_hover_text("Forward")
                            .clicked().then(|| {
                                if let Err(e) = forward() {
                                    self.current_status = e.to_string();
                                } else {
                                    self.current_status = "Loaded".to_string();
                                }
                            });
                        ui.add_space(1.0);
                        let button_width = 25.0;
                        let text_edit_width = ui.available_width() - button_width;
                        let response = ui.add_sized([text_edit_width.max(0.0), 20.0], egui::TextEdit::singleline(&mut self.current_location));
                        if response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.current_status = "Loading...".to_string();
                            self.current_page = go(self.current_location.clone());
                            self.child_window_id = 2.into(); // hide child window
                            self.child_window = None;
                            for tab in &mut self.tabs {
                                if tab.label == self.current_tab {
                                    tab.back.push(tab.location.clone());
                                    tab.forward.clear(); // clear forward history
                                    tab.location = self.current_location.clone();
                                    tab.contents = self.current_page.clone();
                                    tab.label = get_heading(tab.contents.clone());
                                    self.current_tab = tab.label.clone();
                                    break;
                                }
                            }

                            self.current_status = "Loaded".to_string();
                        }
                        ui.add_space(1.0);

                        ui.button(egui_material_icons::icons::ICON_KEYBOARD_DOUBLE_ARROW_RIGHT)
                            .on_hover_text("Go")
                            .clicked()
                            .then(|| {
                                self.current_status = "Loading...".to_string();
                                self.current_page = go(self.current_location.clone());
                                self.child_window_id = 2.into(); // hide child window
                                self.child_window = None;
                                self.current_status = "Loaded".to_string();
                            });

                    });
                });

                egui::TopBottomPanel::bottom("bottom_panel").show(state.egui_renderer.context(), |ui| {
                    let mut status_display: String = "Status: ".to_owned();
                    let status: &str = self.current_status.as_str();
                    status_display.push_str(status);
                    ui.label(status_display);
                });

                egui::SidePanel::left("side_panel").show(state.egui_renderer.context(), |ui| {
                    ui.separator();
                    for tab in &self.tabs {
                        if ui.button(&tab.label).clicked() {
                            self.current_location = tab.location.clone();
                            self.current_page = tab.contents.clone();
                            self.current_tab = tab.label.clone();
                        }
                    }

                    if ui.button("+").clicked() {
                        let new_tab = Tab {
                            label: "New Tab".to_owned(),
                            location: "https://example.com".to_owned(),
                            status: "Loaded".to_owned(),
                            contents: "".to_owned(),
                            back: Vec::new().to_owned(),
                            forward: Vec::new().to_owned(),
                        };
                        self.tabs.push(new_tab.clone());
                        self.current_tab = new_tab.label.clone();
                        self.current_location = new_tab.location.clone();
                        self.current_page = new_tab.contents.clone();
                    }
                });

                if self.child_window_id != 2.into() {
                    //println!("Child window is open");
                } else {
                    egui::CentralPanel::default().show(state.egui_renderer.context(), |ui| {

                        let binding = self.current_page.clone();
                        let markdown = binding.as_str();
                        let mut all_links: Vec<String> = Vec::new();

                        let mut cache = CommonMarkCache::default();

                        ui.style_mut().url_in_tooltip = true;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let parser = pulldown_cmark::Parser::new(markdown);
                            for event in parser {

                                match event {
                                    pulldown_cmark::Event::Start(contents) => {
                                        match contents {
                                            pulldown_cmark::Tag::Link{link_type: _, dest_url: url, title: _, id: _} => {
                                                cache.add_link_hook(url.to_string());
                                                all_links.push(url.to_string());
                                            },
                                            _ => {
                                            }
                                        }
                                    },
                                    _ => {
                                    }
                                }
                            }
                            ui.style_mut().url_in_tooltip = true;
                            CommonMarkViewer::new().show(ui, &mut cache, markdown);
                            for link in all_links {
                                if cache.get_link_hook(&link) == Some(true) {
                                    println!("Link was clicked {link}");
                                    self.current_location = link.clone();
                                    self.current_status = "Loading...".to_string();
                                    self.current_page = go(self.current_location.clone());
                                    self.child_window_id = 2.into(); // hide child window
                                    self.child_window = None;
                                    self.current_status = "Loaded".to_string();
                                }
                                //ui.hyperlink_to(link, link);
                            }


                        });
                    });
                }
                // end of egui browser window



            state.egui_renderer.end_frame_and_draw(
                &state.device,
                &state.queue,
                &mut encoder,
                window,
                &surface_view,
                screen_descriptor,
            );
        }

        state.queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = event_loop
            .create_window(Window::default_attributes())
            .unwrap();
        pollster::block_on(self.set_window(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        // let egui render to process the event first
        self.state
            .as_mut()
            .unwrap()
            .egui_renderer
            .handle_input(self.window.as_ref().unwrap(), &event);

        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                self.handle_redraw();
                self.parent_window_id = window_id;

                self.window.as_ref().unwrap().request_redraw();
                if self.child_window_id != 2.into() {
                    fill::fill_window(self.child_window.as_ref().unwrap());
                    self.child_window.as_ref().unwrap().request_redraw();
                }
                //self.child_window.as_ref().unwrap().request_redraw();

            }
            WindowEvent::Resized(new_size) => {
                self.handle_resized(new_size.width, new_size.height);
            }
            //WindowEvent::KeyboardInput {
            //    event:
            //        KeyEvent {
            //            physical_key: PhysicalKey::Code(KeyCode::KeyM),
            //            state: ElementState::Pressed,
            //            repeat: false,
            //            ..
            //        },
            //    ..
            //} => {
            //    println!("M key pressed");
            //    //let child_window = spawn_child_window(&Arc::try_unwrap(self.window.unwrap().unwrap(), event_loop);
            //    self.child_window = Some(spawn_child_window(self.window.as_ref().unwrap().as_ref(), event_loop));
            //    let child_id = self.child_window.as_ref().unwrap().id();
            //    println!("Child window created with id: {child_id:?}");
            //    self.child_window_id = child_id;
            //},

            WindowEvent::CursorMoved { position, .. } => {

                if self.parent_window_id != 1.into() {
                    self.pointer_pos
                        .insert(self.parent_window_id, (position.x, position.y));
                    if let Some(proxy) = self.proxies.get(&window_id) {
                        proxy.pointer_move(wasi_surface_wasmtime::PointerEvent {
                            x: position.x,
                            y: position.y,
                        });
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers.insert(window_id, modifiers.state());
            }
            WindowEvent::KeyboardInput { event: input, .. } => {
                match input {
                    KeyEvent {
                        physical_key: PhysicalKey::Code(KeyCode::KeyM),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    } => {

                        println!("M key pressed");
                        //let child_window = spawn_child_window(&Arc::try_unwrap(self.window.unwrap().unwrap(), event_loop);
                        self.child_window = Some(spawn_child_window(self.window.as_ref().unwrap().as_ref(), event_loop));
                        let child_id = self.child_window.as_ref().unwrap().id();
                        println!("Child window created with id: {child_id:?}");
                        self.child_window_id = child_id;
                    },
                    _ => {}
                }
                let modifiers = self.modifiers.get(&window_id).unwrap();
                let event = wasi_surface_wasmtime::KeyEvent {
                    key: match input.physical_key {
                        winit::keyboard::PhysicalKey::Code(code) => code.try_into().ok(),
                        winit::keyboard::PhysicalKey::Unidentified(_) => None,
                    },
                    text: match input.logical_key {
                        winit::keyboard::Key::Character(char) => Some(char.to_string()),
                        winit::keyboard::Key::Named(_)
                        | winit::keyboard::Key::Unidentified(_)
                        | winit::keyboard::Key::Dead(_) => None,
                    },
                    alt_key: modifiers.alt_key(),
                    ctrl_key: modifiers.control_key(),
                    meta_key: modifiers.super_key(),
                    shift_key: modifiers.shift_key(),
                };
                if let Some(proxy) = self.proxies.get(&window_id) {
                    match input.state {
                        ElementState::Pressed => {
                            proxy.key_down(event);
                        }
                        ElementState::Released => {
                            proxy.key_up(event);
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, .. } => {
                let (pointer_x, pointer_y) = self.pointer_pos.get(&window_id).unwrap();
                let event = wasi_surface_wasmtime::PointerEvent {
                    x: *pointer_x,
                    y: *pointer_y,
                };
                if let Some(proxy) = self.proxies.get(&window_id) {
                    match state {
                        ElementState::Pressed => {
                            proxy.pointer_down(event);
                        }
                        ElementState::Released => {
                            proxy.pointer_up(event);
                        }
                    }
                }
            }
            WindowEvent::Resized(new_size) => {
                if let Some(proxy) = self.proxies.get(&window_id) {
                    proxy.canvas_resize(wasi_surface_wasmtime::ResizeEvent {
                        height: new_size.height,
                        width: new_size.width,
                    });
                }
            }

            _ => (),
        }
    }
}

fn spawn_child_window(parent: &Window, event_loop: &ActiveEventLoop) -> Window {
    let parent = parent.raw_window_handle().unwrap();
    let mut window_attributes = Window::default_attributes()
        .with_title("child window")
        .with_inner_size(LogicalSize::new(400.0f32, 400.0f32))
        .with_position(Position::Logical(LogicalPosition::new(300.0, 300.0)))
        .with_visible(true);
    // `with_parent_window` is unsafe. Parent window must be a valid window.
    window_attributes = unsafe { window_attributes.with_parent_window(Some(parent)) };

    event_loop.create_window(window_attributes).unwrap()
}

pub fn back() -> Result<(), String> {
    // Implement back navigation logic here
    // For now, just return Ok
    println!("Back button pressed");
    Ok(())
}
pub fn forward() -> Result<(), String> {
    // Implement back navigation logic here
    // For now, just return Ok
    println!("Forward button pressed");
    Ok(())
}
pub fn go(url: String) -> String {
    // Implement go logic here
    println!("Going to URL: {}", url);
    println!("Go button pressed");
    let resp = navigate(url);
    return resp;
}

pub fn navigate(location: String) -> String {
    // Implement navigation logic here
    // For now, just return Ok
    println!("Navigating to URL: {}", location);
    let resp = reqwest::blocking::get(&location)
        .and_then(|r| r.text())
        .map_err(|e| e.to_string());

    // if the location ends with .wasm we'll try loading it
    if location.ends_with(".wasm") {
        let wasm_bytes = resp.unwrap_or_else(|_| {
            eprintln!("Failed to load wasm file at {}", location);
            return "Failed to load wasm file".to_string();
        });
        // save wasm_bytes to a file
        let wasm_path = format!("./{}.wasm", location.replace("https://", "").replace("/", "_"));
        std::fs::write(&wasm_path, wasm_bytes).unwrap_or_else(|_| {
            eprintln!("Failed to write wasm file to {}", wasm_path);
        });
        //env_logger::builder()
        //    .filter_level(log::LevelFilter::Info)
        //    .init();

        let mut config = Config::default();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = match Engine::new(&config) {
            Ok(engine) => engine,
            Err(e) => {
                println!("Failed to create Wasmtime engine: {}", e);
                return "Failed to create Wasmtime engine".to_string();
            }
        };
        let mut linker: Linker<HostState> = Linker::new(&engine);

        wasi_webgpu_wasmtime::add_to_linker(&mut linker);
        wasi_frame_buffer_wasmtime::add_to_linker(&mut linker);
        wasi_graphics_context_wasmtime::add_to_linker(&mut linker);
        wasi_surface_wasmtime::add_to_linker(&mut linker);

        fn type_annotate<F>(val: F) -> F
        where
            F: Fn(&mut HostState) -> &mut dyn ExampleImports,
        {
            val
        }
        let closure = type_annotate::<_>(|t| t);
        Example::add_to_linker_imports_get_host(&mut linker, closure);

        println!("about to call wasi_winit_event_loop");

        //let main_thread_proxy = WasiWinitEventLoopProxy {
        //    proxy: event_loop.event_loop.create_proxy(),
        //};

        //let host_state = HostState::new(main_thread_proxy);

        //let mut store = Store::new(&engine, host_state);

        //let component =
        //    match Component::from_file(&engine, &wasm_path).context("Component file not found") {
        //        Ok(component) => component,
        //        Err(e) => {
        //            println!("Failed to load component: {}", e);
        //            return "Failed to load component".to_string();
        //        }
        //    };


        //tokio::spawn(async move {
        //    let instance = Example::instantiate_async(&mut store, &component, &linker)
        //        .await
        //        .unwrap();
        //    instance.call_start(&mut store).await.unwrap();
        //});

        //return "Wasm file loaded and running".to_string();
        return;
    }
    //println!("{:#?}", resp);
    //self.current_status = "Loaded".to_string();
    //return resp.unwrap_or_else(|_| "Failed to load page".to_string());
}

fn get_heading(contents: String) -> String {
    let mut heading = String::new();
    let mut in_heading = false;
    let parser = PullDownParser::new_ext(&contents, Options::empty());
    for event in parser {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading { .. }) => {
                in_heading = true;
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if in_heading {
                    break;
                }
            }
            pulldown_cmark::Event::Text(text) => {
                if in_heading {
                    heading.push_str(&text);
                }
            }
            _ => {}
        }
    }
    heading
}
