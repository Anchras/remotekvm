// remotekvm-client
//
// Native desktop application for RemoteKVM SaaS.
//
// Stack: winit + wgpu + egui
//
// UI Flow:
//   1. Login screen (WorkOS AuthKit via browser)
//   2. Dashboard (list of machines with online status)
//   3. Connection view (video stream + input capture)

use anyhow::Result;
use egui_wgpu::ScreenDescriptor;
use egui_winit::EventResponse;
use winit::{
    event::Event,
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};

mod app;
mod auth;
mod config;
mod server_client;
mod webrtc_client;

use app::{App, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_client=debug".into()),
        )
        .init();

    tracing::info!("remotekvm-client starting");

    let event_loop = EventLoop::new()?;
    let window = std::sync::Arc::new(
        WindowBuilder::new()
            .with_title("RemoteKVM")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0))
            .build(&event_loop)?,
    );

    // Initialize wgpu
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = instance.create_surface(window.clone())?;

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .ok_or_else(|| anyhow::anyhow!("No suitable GPU adapter found"))?;

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            label: Some("Device"),
        },
        None,
    ))?;

    let surface_caps = surface.get_capabilities(&adapter);
    let surface_format = surface_caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(surface_caps.formats[0]);

    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: window.inner_size().width,
        height: window.inner_size().height,
        present_mode: surface_caps.present_modes[0],
        alpha_mode: surface_caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    // Initialize egui
    let egui_context = egui::Context::default();
    let mut egui_state = egui_winit::State::new(
        egui_context.clone(),
        egui::ViewportId::default(),
        &*window,
        None,
        None,
    );

    let mut renderer = egui_wgpu::Renderer::new(
        &device,
        surface_format,
        None,
        1,
    );

    // Initialize app state
    let mut app = App::new();

    // Event loop
    event_loop.run(move |event, window_target| {
        window_target.set_control_flow(ControlFlow::Poll);

        match event {
            Event::WindowEvent { event, .. } => {
                let response = egui_state.on_window_event(&*window, &event);
                if response.consumed {
                    return;
                }

                match event {
                    winit::event::WindowEvent::Resized(new_size) => {
                        if new_size.width > 0 && new_size.height > 0 {
                            config.width = new_size.width;
                            config.height = new_size.height;
                            surface.configure(&device, &config);
                        }
                    }
                    winit::event::WindowEvent::CloseRequested => {
                        window_target.exit();
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                window.request_redraw();
            }
            Event::WindowEvent {
                event: winit::event::WindowEvent::RedrawRequested,
                ..
            } => {
                let raw_input = egui_state.take_egui_input(&*window);
                let egui_output = egui_context.run(raw_input, |ctx| {
                    app.ui(ctx);
                });

                egui_state.handle_platform_output(&*window, egui_output.platform_output);

                let clipped_primitives = egui_context.tessellate(
                    egui_output.shapes,
                    egui_output.pixels_per_point,
                );

                let screen_descriptor = ScreenDescriptor {
                    size_in_pixels: [config.width, config.height],
                    pixels_per_point: egui_output.pixels_per_point,
                };

                let frame = match surface.get_current_texture() {
                    Ok(frame) => frame,
                    Err(_) => {
                        surface.configure(&device, &config);
                        surface
                            .get_current_texture()
                            .expect("Failed to acquire next surface texture!")
                    }
                };

                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());

                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Render Encoder"),
                });

                {
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("egui_render_pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.1,
                                    g: 0.1,
                                    b: 0.1,
                                    a: 1.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });

                    renderer.render(&mut render_pass, &clipped_primitives, &screen_descriptor);
                }

                queue.submit(std::iter::once(encoder.finish()));
                frame.present();

                window.request_redraw();
            }
            _ => {}
        }
    })?;

    Ok(())
}
