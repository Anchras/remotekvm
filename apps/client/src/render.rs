//! Headless render verification.
//!
//! Renders one egui frame to an offscreen RGBA texture with wgpu and reads the
//! pixels back. This exercises the exact tessellate → upload-buffers → render
//! path used by the live event loop (the path whose `RedrawRequested` arm was
//! previously unreachable), without needing a window or Screen Recording
//! permission in tests. Runtime helpers in this module convert decoded video
//! frames into egui textures so egui-wgpu uploads them through the live render
//! path.

use anyhow::{anyhow, Result};

use crate::media::{DecodedVideoFrame, VideoPixelFormat};

#[derive(Default)]
pub struct VideoTexture {
    texture: Option<egui::TextureHandle>,
}

impl VideoTexture {
    pub fn show_frame(
        &mut self,
        ui: &mut egui::Ui,
        frame: &DecodedVideoFrame,
    ) -> Result<egui::Response> {
        let image = decoded_frame_to_color_image(frame)?;
        let options = egui::TextureOptions::LINEAR;
        if let Some(texture) = &mut self.texture {
            texture.set(image, options);
        } else {
            self.texture = Some(ui.ctx().load_texture("remote-video", image, options));
        }

        let texture = self
            .texture
            .as_ref()
            .ok_or_else(|| anyhow!("video texture was not created"))?;
        let size = fitted_video_size(
            egui::vec2(frame.width as f32, frame.height as f32),
            ui.available_size(),
        );
        Ok(ui.add(egui::Image::new(texture).fit_to_exact_size(size)))
    }
}

fn fitted_video_size(video: egui::Vec2, available: egui::Vec2) -> egui::Vec2 {
    if video.x <= 0.0 || video.y <= 0.0 {
        return egui::vec2(1.0, 1.0);
    }
    let max_width = available.x.max(1.0);
    let max_height = available.y.max(1.0);
    let scale = (max_width / video.x).min(max_height / video.y).min(1.0);
    egui::vec2((video.x * scale).max(1.0), (video.y * scale).max(1.0))
}

fn decoded_frame_to_color_image(frame: &DecodedVideoFrame) -> Result<egui::ColorImage> {
    let rgba = decoded_frame_to_rgba(frame)?;
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [frame.width as usize, frame.height as usize],
        &rgba,
    ))
}

fn decoded_frame_to_rgba(frame: &DecodedVideoFrame) -> Result<Vec<u8>> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    if width == 0 || height == 0 {
        anyhow::bail!("decoded video frame has zero dimensions");
    }
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("decoded video frame dimensions overflow"))?;
    match frame.format {
        VideoPixelFormat::Rgba8 => {
            let expected = pixels * 4;
            if frame.data.len() != expected {
                anyhow::bail!(
                    "RGBA frame size mismatch: got {}, expected {expected}",
                    frame.data.len()
                );
            }
            Ok(frame.data.to_vec())
        }
        VideoPixelFormat::Bgra8 => {
            let expected = pixels * 4;
            if frame.data.len() != expected {
                anyhow::bail!(
                    "BGRA frame size mismatch: got {}, expected {expected}",
                    frame.data.len()
                );
            }
            let mut rgba = Vec::with_capacity(expected);
            for bgra in frame.data.chunks_exact(4) {
                rgba.extend_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
            }
            Ok(rgba)
        }
        VideoPixelFormat::Nv12 => nv12_to_rgba(&frame.data, width, height),
        VideoPixelFormat::I420 => i420_to_rgba(&frame.data, width, height),
    }
}

fn nv12_to_rgba(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let y_len = width * height;
    let uv_height = (height + 1) / 2;
    let expected = y_len + width * uv_height;
    if data.len() < expected {
        anyhow::bail!(
            "NV12 frame too small: got {}, expected at least {expected}",
            data.len()
        );
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let luma = data[y * width + x];
            let uv_index = y_len + (y / 2) * width + (x / 2) * 2;
            let u = data[uv_index];
            let v = data[uv_index + 1];
            push_yuv_as_rgba(&mut rgba, luma, u, v);
        }
    }
    Ok(rgba)
}

fn i420_to_rgba(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let y_len = width * height;
    let chroma_width = (width + 1) / 2;
    let chroma_height = (height + 1) / 2;
    let chroma_len = chroma_width * chroma_height;
    let u_offset = y_len;
    let v_offset = u_offset + chroma_len;
    let expected = v_offset + chroma_len;
    if data.len() < expected {
        anyhow::bail!(
            "I420 frame too small: got {}, expected at least {expected}",
            data.len()
        );
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let chroma_index = (y / 2) * chroma_width + (x / 2);
            push_yuv_as_rgba(
                &mut rgba,
                data[y * width + x],
                data[u_offset + chroma_index],
                data[v_offset + chroma_index],
            );
        }
    }
    Ok(rgba)
}

fn push_yuv_as_rgba(out: &mut Vec<u8>, y: u8, u: u8, v: u8) {
    let y = y as f32;
    let u = u as f32 - 128.0;
    let v = v as f32 - 128.0;
    let r = y + 1.402 * v;
    let g = y - 0.344_136 * u - 0.714_136 * v;
    let b = y + 1.772 * u;
    out.extend_from_slice(&[clamp_rgb(r), clamp_rgb(g), clamp_rgb(b), 255]);
}

fn clamp_rgb(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
use egui::Context;

/// Render `run_ui` to a `width`×`height` RGBA8 texture; return the raw pixels.
#[cfg(test)]
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
    use bytes::Bytes;

    #[test]
    fn bgra_frame_converts_to_rgba() {
        let frame = DecodedVideoFrame {
            width: 1,
            height: 1,
            format: VideoPixelFormat::Bgra8,
            pts: None,
            data: Bytes::from_static(&[10, 20, 30, 255]),
        };

        assert_eq!(
            decoded_frame_to_rgba(&frame).unwrap(),
            vec![30, 20, 10, 255]
        );
    }

    #[test]
    fn nv12_neutral_chroma_converts_luma_to_gray() {
        let frame = DecodedVideoFrame {
            width: 2,
            height: 2,
            format: VideoPixelFormat::Nv12,
            pts: None,
            data: Bytes::from_static(&[64, 128, 192, 255, 128, 128]),
        };

        assert_eq!(
            decoded_frame_to_rgba(&frame).unwrap(),
            vec![64, 64, 64, 255, 128, 128, 128, 255, 192, 192, 192, 255, 255, 255, 255, 255,]
        );
    }

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
