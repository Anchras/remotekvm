//! Headless render verification.
//!
//! Renders one egui frame to an offscreen RGBA texture with wgpu and reads the
//! pixels back. This exercises the exact tessellate → upload-buffers → render
//! path used by the live event loop (the path whose `RedrawRequested` arm was
//! previously unreachable), without needing a window or Screen Recording
//! permission. Test-only.

#![cfg(test)]

use egui::Context;

/// Render `run_ui` to a `width`×`height` RGBA8 texture; return the raw pixels.
pub fn render_to_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut egui_wgpu::Renderer,
    ctx: &Context,
    width: u32,
    height: u32,
    run_ui: impl FnOnce(&Context),
) -> Vec<u8> {
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(width as f32, height as f32),
        )),
        ..Default::default()
    };
    let output = ctx.run(raw_input, run_ui);
    let primitives = ctx.tessellate(output.shapes, 1.0);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: 1.0,
    };

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for (id, delta) in &output.textures_delta.set {
        renderer.update_texture(device, queue, *id, delta);
    }
    renderer.update_buffers(device, queue, &mut encoder, &primitives, &screen);
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("offscreen_pass"),
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
        renderer.render(&mut pass, &primitives, &screen);
    }

    let bytes_per_row = width * 4; // 256-aligned for width multiples of 64
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    let data = slice.get_mapped_range().to_vec();
    buffer.unmap();
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("headless"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .ok()?;
        Some((device, queue))
    }

    #[tokio::test]
    async fn egui_frame_renders_non_blank_content() {
        let Some((device, queue)) = headless_device().await else {
            eprintln!("no GPU adapter available; skipping render test");
            return;
        };
        let ctx = Context::default();
        let mut renderer =
            egui_wgpu::Renderer::new(&device, wgpu::TextureFormat::Rgba8Unorm, None, 1);

        let (w, h) = (256u32, 256u32);
        // Draw a representative login-style frame (themed panel + heading +
        // button) — the same egui→wgpu path the live app uses.
        let pixels = render_to_rgba(&device, &queue, &mut renderer, &ctx, w, h, |ctx| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgb(15, 23, 42);
            ctx.set_visuals(visuals);
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("RemoteKVM");
                ui.label("Sign in to access your remote machines");
                let _ = ui.button("Continue with WorkOS");
            });
        });

        assert_eq!(pixels.len(), (w * h * 4) as usize);

        // The frame must not be a single flat color: a rendered panel + text
        // produces a spread of distinct pixel values. (A never-drawn frame
        // would be uniformly the clear color — the symptom of the old bug.)
        let mut distinct = std::collections::HashSet::new();
        for px in pixels.chunks_exact(4) {
            distinct.insert([px[0], px[1], px[2]]);
            if distinct.len() > 8 {
                break;
            }
        }
        assert!(
            distinct.len() > 8,
            "render produced a near-uniform image ({} distinct colors) — \
             egui content was not drawn",
            distinct.len()
        );

        // And the themed panel fill (slate-900-ish) should dominate, proving the
        // CentralPanel actually painted rather than leaving the clear color.
        let clear = [26u8, 26, 26]; // 0.1 * 255
        let panelish = pixels
            .chunks_exact(4)
            .filter(|px| {
                let near = |a: u8, b: u8| (a as i16 - b as i16).abs() < 16;
                !(near(px[0], clear[0]) && near(px[1], clear[1]) && near(px[2], clear[2]))
            })
            .count();
        assert!(
            panelish > (w * h / 2) as usize,
            "themed panel did not cover the frame ({panelish} non-clear px)"
        );
    }
}
