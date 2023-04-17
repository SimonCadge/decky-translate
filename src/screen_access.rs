use std::{time::Instant, io, mem, ops::Index};

use bytemuck::{Pod, Zeroable};
use html_parser::{Node};
use pollster::block_on;
use tokio::{task, runtime::{Runtime, self}};
use wgpu::{util::DeviceExt, BufferUsages};
use wgpu_glyph::{GlyphBrush, ab_glyph::{self, Font}, GlyphBrushBuilder, Text, Layout, GlyphCruncher, OwnedSection, Section, FontId};
use winit::{
    event::*,
    event_loop::{ControlFlow, EventLoop},
    window::{WindowBuilder, Window}, dpi::PhysicalPosition,
};
use screenshots::Screen;

use crate::{ocr, supported_languages::SupportedLanguages};

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 3],
}

impl Vertex {
    const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x3];
    fn desc<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PixelPoint {
    x: f32,
    y: f32,
}

impl PixelPoint {
    fn new(x: f32, y: f32) -> Self {
        Self {
            x,
            y
        }
    }

    fn to_normalized_coordinate(self, screen_max_point: PixelPoint) -> [f32; 2] {
        let x = (self.x / (screen_max_point.x / 2.0)) - 1.0;
        let y = (self.y / (screen_max_point.y / 2.0)) - 1.0;
        [x, y]
    }
}

impl From<(f32, f32)> for PixelPoint {
    fn from(pos: (f32, f32)) -> Self {
        Self { x: pos.0, y: pos.1 }
    }
}

impl From<ab_glyph::Point> for PixelPoint {
    fn from(pos: ab_glyph::Point) -> Self {
        Self {
            x: pos.x,
            y: pos.y
        }
    }
}

#[derive(Debug, Clone)]
struct BboxWord {
    text: String,
    min: PixelPoint,
    max: PixelPoint,
    is_highlighted: bool,
    confidence: f32,
    language: SupportedLanguages,
}

impl BboxWord {
    fn is_within_bounds(&mut self, position: &PhysicalPosition<f64>) -> bool {
        let cursor_x: f32 = position.x as f32;
        let cursor_y: f32 = position.y as f32;
        return cursor_x > self.min.x && cursor_x <= self.max.x
            && cursor_y > self.min.y && cursor_y <= self.max.y;
    }

    fn to_text(&self, scale: f32) -> Text {
        return Text::default()
            .with_text(&self.text)
            .with_scale(scale)
            .with_color(self.get_colour())
            .with_font_id(if self.language == SupportedLanguages::Eng {FontId(0)} else {FontId(1)});
    }

    fn get_colour(&self) -> [f32; 4] {
        if self.is_highlighted {
            return [0.0, 1.0, 0.0, 1.0];
        } else if self.confidence < 90.0 {
            return [1.0, 0.0, 0.0, 1.0];
        } else {
            return [0.0, 0.0, 0.0, 1.0];
        }
    }
}

struct BboxLine {
    words: Vec<BboxWord>,
    is_highlighted: bool,
}

impl BboxLine {
    fn new(words: Vec<BboxWord>) -> Self {
        return Self {
            words,
            is_highlighted: false,
        }
    }

    fn get_min(&self) -> PixelPoint {
        let smallest_x = self.words.iter().map(|word| word.min.x).min_by(|a, b| a.total_cmp(b)).unwrap();
        let smallest_y = self.words.iter().map(|word| word.min.y).min_by(|a, b| a.total_cmp(b)).unwrap();
        return PixelPoint::new(smallest_x, smallest_y);
    }

    fn get_max(&self) -> PixelPoint {
        let largest_x = self.words.iter().map(|word| word.max.x).max_by(|a, b| a.total_cmp(b)).unwrap();
        let largest_y = self.words.iter().map(|word| word.max.y).max_by(|a, b| a.total_cmp(b)).unwrap();
        return PixelPoint::new(largest_x, largest_y);
    }

    fn get_scale(&self) -> f32 {
        return self.get_max().y - self.get_min().y;
    }

    fn to_section(&self) -> OwnedSection {
        let text = self.words.iter().map(|word| word.to_text(self.get_scale())).collect();
        return Section::default()
            .with_screen_position((self.get_min().x, self.get_min().y))
            .with_layout(Layout::default())
            .with_text(text)
            .to_owned();
    }

    fn to_vertices(&self, screen_max_point: PixelPoint, offset: u32) -> (Vec<Vertex>, Vec<u32>) {
        let min = self.get_min();
        let max = self.get_max();
        let verticies = vec![
            Vertex { //top left
                position: min.to_normalized_coordinate(screen_max_point),
                color: [1.0, 1.0, 1.0],
            },
            Vertex { //top right
                position: PixelPoint::new(max.x, min.y).to_normalized_coordinate(screen_max_point),
                color: [1.0, 1.0, 1.0],
            },
            Vertex { //bottom left
                position: PixelPoint::new(min.x, max.y).to_normalized_coordinate(screen_max_point),
                color: [1.0, 1.0, 1.0],
            },
            Vertex { //bottom right
                position: max.to_normalized_coordinate(screen_max_point),
                color: [1.0, 1.0, 1.0],
            },
        ];
        let indices = vec![
            offset + 0, offset + 1, offset + 2,
            offset + 2, offset + 1, offset + 3
        ];
        return (verticies, indices);
    }

    fn is_within_bounds(&mut self, position: PhysicalPosition<f64>) -> bool {
        let cursor_x: f32 = position.x as f32;
        let cursor_y: f32 = position.y as f32;
        return cursor_x > self.get_min().x && cursor_x <= self.get_max().x
            && cursor_y > self.get_min().y && cursor_y <= self.get_max().y;
    }
}

struct State {
    surface: wgpu::Surface,
    device: wgpu::Device,
    queue: wgpu::Queue,
    staging_belt: wgpu::util::StagingBelt,
    config: wgpu::SurfaceConfiguration,
    size: winit::dpi::PhysicalSize<u32>,
    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    window: Window,
    glyph_brush: GlyphBrush<()>,
    tokio_runtime: Runtime,
    ocr_job: Option<task::JoinHandle<Result<String, io::Error>>>,
    ocr_text: Option<Vec<BboxLine>>,
}

impl State {
    // Creating some of the wgpu types requires async code
    async fn new(window: Window) -> Self {
        let size = window.inner_size();

        // The instance is a handle to our GPU
        // Backends::all => Vulkan + Metal + DX12 + Browser WebGPU
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            dx12_shader_compiler: Default::default(),
        });
        
        // # Safety
        //
        // The surface needs to live as long as the window that created it.
        // State owns the window so this should be safe.
        let surface = unsafe { instance.create_surface(&window) }.unwrap();

        let adapter = instance
        .enumerate_adapters(wgpu::Backends::all())
        .filter(|adapter| {
            // Check if this adapter supports our surface
            adapter.is_surface_supported(&surface)
        })
        .next()
        .unwrap();

        let (device, queue) = adapter.request_device(
            &wgpu::DeviceDescriptor {
                features: wgpu::Features::empty(),
                // WebGL doesn't support all of wgpu's features, so if
                // we're building for the web we'll have to disable some.
                limits: if cfg!(target_arch = "wasm32") {
                    wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    wgpu::Limits::default()
                },
                label: None,
            },
            None, // Trace path
        ).await.unwrap();

        let surface_caps = surface.get_capabilities(&adapter);

        // Shader code in this tutorial assumes an sRGB surface texture. Using a different
        // one will result all the colors coming out darker. If you want to support non
        // sRGB surfaces, you'll need to account for that when drawing to the frame.
        let surface_format = surface_caps.formats.iter()
            .copied()
            .filter(|f| f.describe().srgb)
            .next()
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: surface_caps.present_modes[0],
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { 
            label: Some("Shader"), 
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()), 
        });

        let render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Render Pipeline Layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[
                    Vertex::desc()
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { 
                topology: wgpu::PrimitiveTopology::TriangleList, 
                strip_index_format: None, 
                front_face: wgpu::FrontFace::Ccw, 
                cull_mode: Some(wgpu::Face::Back), 
                unclipped_depth: false, 
                polygon_mode: wgpu::PolygonMode::Fill, 
                conservative: false },
            depth_stencil: None,
            multisample: wgpu::MultisampleState { 
                count: 1, 
                mask: !0, 
                alpha_to_coverage_enabled: false 
            },
            multiview: None,
        });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Vertex Buffer"),
            size: 10000 * mem::size_of::<Vertex>() as u64, //Assuming we never need more than 1000 vertices
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let index_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Index Buffer"),
            size: 10000 * mem::size_of::<u16>() as u64,
            usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Prepare glyph_brush
        let simhei = ab_glyph::FontArc::try_from_slice(include_bytes!(
            "SimHei.ttf"
        )).unwrap();
        let inconsolata = ab_glyph::FontArc::try_from_slice(include_bytes!(
            "Inconsolata-Regular.ttf"
        )).unwrap();

        let glyph_brush = GlyphBrushBuilder::using_fonts(vec![inconsolata, simhei])
            .build(&device, surface_format);

        Self {
            window,
            surface,
            device,
            queue,
            staging_belt: wgpu::util::StagingBelt::new(1024),
            config,
            size,
            render_pipeline,
            vertex_buffer,
            index_buffer,
            glyph_brush,
            tokio_runtime: runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("ocr_worker")
                .build()
                .unwrap(),
            ocr_job: None,
            ocr_text: None,
        }
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn input(&mut self, _event: &WindowEvent) -> bool {
        false
    }

    fn update(&mut self) {
        if let Some(running_job) = &self.ocr_job {
            running_job.abort();
            self.ocr_job = None;
            self.ocr_text = None;
        }
        let window_size = self.window.inner_size();
        let window_inner_position = self.window.inner_position().unwrap();
        self.ocr_job = Some(self.tokio_runtime.spawn(async move {
            let start_time = Instant::now();
            let screen = Screen::from_point(window_inner_position.x, window_inner_position.y).unwrap();
            let display_position = screen.display_info;
            let image = screen.capture_area(window_inner_position.x - display_position.x, window_inner_position.y - display_position.y, window_size.width, window_size.height).unwrap();
            let buffer = image.buffer();
            println!("Screenshot took {} ms", start_time.elapsed().as_millis());
            return Ok(ocr::execute_ocr(buffer));
        }));
    }

    fn check_running_job(&mut self) {
        if self.ocr_job.is_some() {
            let running_job = self.ocr_job.as_mut().unwrap();
            if running_job.is_finished() {
                let hocr = block_on(running_job).unwrap().unwrap();
                println!("{}", hocr);
                self.ocr_job = None;
                self.ocr_text = Some(self.nodes_to_lines(&html_parser::Dom::parse(&hocr).unwrap().children));
                self.render().unwrap();
            }
        }
    }

    fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: true,
                    },
                })],
                depth_stencil_attachment: None,
            });

            render_pass.set_pipeline(&self.render_pipeline);

            if let Some(lines) = &self.ocr_text {
                let mut vertices: Vec<Vertex> = Vec::with_capacity(10000 * mem::size_of::<Vertex>());
                let mut indices: Vec<u32> = Vec::with_capacity(10000 * mem::size_of::<u32>());
                let mut offset = 0;
                let mut num_indices = 0;
                let screen_size = PixelPoint::new(self.config.width as f32, self.config.height as f32);
                for line in lines {
                    let (mut line_vertices, mut line_indices) = line.to_vertices(screen_size, offset);
                    offset += line_vertices.len() as u32;
                    vertices.append(&mut line_vertices);
                    num_indices += line_indices.len() as u32;
                    indices.append(&mut line_indices);
                }
                self.queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
                self.queue.write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));

                render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..num_indices, 0, 0..1);
            }
        }

        if let Some(lines) = &self.ocr_text {
            for line in lines {
                let section = &line.to_section();
                self.glyph_brush.queue(section);
            }
            self.glyph_brush.draw_queued(&self.device, &mut self.staging_belt, &mut encoder, &view, self.size.width, self.size.height).unwrap();
        }
    
        self.staging_belt.finish();
        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        self.staging_belt.recall();
    
        Ok(())
    }

    fn handle_cursor(&mut self, cursor_position: &PhysicalPosition<f64>) {
        if let Some(bbox_lines) = &mut self.ocr_text {
            for line in bbox_lines {
                for bbox_word in &mut line.words {
                    if bbox_word.is_within_bounds(cursor_position) {
                        bbox_word.is_highlighted = true;
                    } else {
                        bbox_word.is_highlighted = false;
                    }
                }
            }
            self.render().unwrap();
        }
    }

    fn handle_click(&self) {
        if let Some(bbox_lines) = &self.ocr_text {
            for line in bbox_lines {
                for bbox_word in &line.words {
                    if bbox_word.is_highlighted {
                        println!("{} - {:?},{:?}", bbox_word.text, bbox_word.min, bbox_word.max);
                    }
                }
            }
        }
    }

    fn nodes_to_lines(&mut self, nodes: &Vec<Node>) -> Vec<BboxLine> {
        let mut lines: Vec<BboxLine> = Vec::new();
        for node in nodes {
            if let html_parser::Node::Element(element) = node {
                if element.classes.contains(&"ocr_line".to_string()) { // is individual line
                    let num_words = element.children.len();
                    let mut words = Vec::with_capacity(num_words);
                    for word in &element.children {
                        if let html_parser::Node::Element(word_element) = word {
                            let title = word_element.attributes["title"].clone().unwrap();
                            let mut parts = title.split(" ");
                            parts.next();
                            let x = parse_bbox_f32(parts.next().unwrap());
                            let y = parse_bbox_f32(parts.next().unwrap());
                            let x2 = parse_bbox_f32(parts.next().unwrap());
                            let y2 = parse_bbox_f32(parts.next().unwrap());
                            parts.next();
                            let confidence = parts.next().unwrap().parse::<f32>().unwrap();
                            let text = get_text_child(&word_element.children);
                            let word = BboxWord {
                                text,
                                min: PixelPoint::new(x, y),
                                max: PixelPoint::new(x2, y2),
                                is_highlighted: false,
                                confidence,
                                language: SupportedLanguages::ChiTra
                            };
                            words.push(word);
                        }
                    }
                    println!("Words {:?}", words);
                    let line = BboxLine::new(words.clone());
                    let section = &line.to_section();
                    let font = self.glyph_brush.fonts().to_vec();
                    for section_glyph in self.glyph_brush.glyphs(section) {
                        println!("{:?}", section_glyph);
                        let glyph = &section_glyph.glyph;
                        let glyph_bounds = font[section_glyph.font_id.0].glyph_bounds(glyph);
                        let i = section_glyph.section_index;
                        words[i].min = PixelPoint::from(glyph_bounds.min);
                        words[i].max = PixelPoint::from(glyph_bounds.max);
                    }
                    let line = BboxLine::new(words);
                    lines.push(line);
                } else { // call recursively until we reach individual words
                    lines.append(&mut self.nodes_to_lines(&node.element().unwrap().children));
                }
            }
        }
        return lines;
    }
    
}

fn get_text_child(nodes: &Vec<Node>) -> String {
    for node in nodes {
        if let html_parser::Node::Text(text) = node {
            return text.to_string();
        } else if let html_parser::Node::Element(element) = node {
            return get_text_child(&element.children);
        }
    }
    return "".to_string();
}

fn parse_bbox_f32(string: &str) -> f32 {
    let parsed = string.chars().filter(|char| char.is_digit(10)).collect::<String>().parse::<f32>().unwrap();
    return parsed / 4.0; //OCR image was upscaled 4x before processing
}

pub async fn screen_entry() {
    env_logger::init();
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new().with_transparent(true).build(&event_loop).unwrap();

    let mut window_state = State::new(window).await;

    event_loop.run(move |event, _, control_flow| {
        match event {
            Event::WindowEvent {
                ref event,
                window_id,
            } if window_id == window_state.window().id() => if !window_state.input(event) {
                match event {
                    WindowEvent::CloseRequested
                    | WindowEvent::KeyboardInput {
                        input:
                            KeyboardInput {
                                state: ElementState::Pressed,
                                virtual_keycode: Some(VirtualKeyCode::Escape),
                                ..
                            },
                        ..
                    } => *control_flow = ControlFlow::Exit,
                    WindowEvent::Resized(physical_size) => {
                        window_state.resize(*physical_size);
                    }
                    WindowEvent::ScaleFactorChanged { new_inner_size, .. } => {
                        window_state.resize(**new_inner_size);
                    }
                    WindowEvent::Moved(_) => {
                        window_state.window().request_redraw();
                    }
                    WindowEvent::CursorMoved { device_id: _, position, modifiers: _ } => {
                        window_state.handle_cursor(position);
                    }
                    WindowEvent::MouseInput { device_id: _, state, button: _, modifiers: _ } => {
                        if let ElementState::Released = state {
                            window_state.handle_click();
                        }
                    }
                    _ => {}
                }
            }
            Event::RedrawRequested(window_id) if window_id == window_state.window().id() => {
                window_state.update();
                match window_state.render() {
                    Ok(_) => {}
                    // Reconfigure the surface if lost
                    Err(wgpu::SurfaceError::Lost) => window_state.resize(window_state.size),
                    // The system is out of memory, we should probably quit
                    Err(wgpu::SurfaceError::OutOfMemory) => *control_flow = ControlFlow::Exit,
                    // All other errors (Outdated, Timeout) should be resolved by the next frame
                    Err(e) => eprintln!("{:?}", e),
                }
            }
            Event::MainEventsCleared => {
                // RedrawRequested will only trigger once, unless we manually
                // request it.
                window_state.check_running_job();
                // state.window().request_redraw();
            }
            _ => {}
        }
    });
}

// https://sotrh.github.io/learn-wgpu/beginner/tutorial2-surface/#render