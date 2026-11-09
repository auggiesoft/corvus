//! The 3D waterfall: the waterfall history as a lit landscape (frequency across, time going back,
//! level as height and colour), drawn on the GPU inside egui's render pass with a depth buffer.
//!
//! The grid has no vertex buffer: the vertex shader makes each cell's two triangles from the vertex
//! index and reads its height from an R32Float texture of the history (one texel per grid point, built by
//! the app from the waterfall), and its colour from the palette as a 256x1 texture. Normals come from the
//! neighbouring heights. Overlays (axes, labels) are drawn with egui's painter, projected with the same
//! matrix (`Camera::project`).

use std::sync::Arc;

use eframe::egui_wgpu::{self, wgpu};
use egui::{Pos2, Rect, pos2};

/// Largest grid drawn: frequency points across, rows back in time.
pub const GX_MAX: usize = 768;
pub const GZ_MAX: usize = 256;
/// World size: frequency spans x -1..1 (left to right), time z 0..-DEPTH (newest at the front, older
/// further from the camera), height 0..HEIGHT. The camera looks along -z, so x isn't mirrored.
pub const DEPTH: f32 = 1.7;
pub const HEIGHT: f32 = 0.55;
/// Multisample anti-aliasing for the whole window (eframe's `multisampling`); the 3D pipeline has to
/// match it. Takes the stair steps off the landscape's ridges and walls.
pub const MSAA: u32 = 4;

/// Gaussian smoothing of the height grid, in linear power so a narrow carrier keeps most of its height
/// (a one-cell spike loses a few dB, not most of its dB) while the noise floor's grass averages out.
/// `sx`/`sz` are the sigmas in grid cells across (frequency) and back (time); 0 leaves that axis alone.
pub fn smooth(h: &mut [f32], gx: usize, gz: usize, sx: f32, sz: f32) {
    if h.len() < gx * gz || gx == 0 || gz == 0 || (sx <= 0.0 && sz <= 0.0) {
        return;
    }
    let kernel = |s: f32| -> Vec<f32> {
        let r = (s * 3.0).ceil() as i32;
        let k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * s * s)).exp()).collect();
        let sum: f32 = k.iter().sum();
        k.into_iter().map(|w| w / sum).collect()
    };
    let mut p: Vec<f32> = h[..gx * gz].iter().map(|&db| 10f32.powf(db * 0.1)).collect();
    let mut tmp = vec![0.0f32; gx * gz];
    if sx > 0.0 {
        let k = kernel(sx);
        let r = (k.len() / 2) as i32;
        for z in 0..gz {
            let row = &p[z * gx..(z + 1) * gx];
            for x in 0..gx {
                tmp[z * gx + x] = k.iter().enumerate().map(|(j, w)| w * row[(x as i32 + j as i32 - r).clamp(0, gx as i32 - 1) as usize]).sum();
            }
        }
        std::mem::swap(&mut p, &mut tmp);
    }
    if sz > 0.0 {
        let k = kernel(sz);
        let r = (k.len() / 2) as i32;
        for z in 0..gz {
            for x in 0..gx {
                tmp[z * gx + x] = k.iter().enumerate().map(|(j, w)| w * p[(z as i32 + j as i32 - r).clamp(0, gz as i32 - 1) as usize * gx + x]).sum();
            }
        }
        std::mem::swap(&mut p, &mut tmp);
    }
    for (o, v) in h.iter_mut().zip(p) {
        *o = 10.0 * v.max(1e-30).log10();
    }
}

const SHADER: &str = r#"
struct U {
    mvp: mat4x4<f32>,
    // lo dB, hi dB, height, unused
    p0: vec4<f32>,
    // grid x, grid z, linear framebuffer (1) or not (0), unused
    p1: vec4<f32>,
    // depth, unused...
    p2: vec4<f32>,
    p3: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var heights: texture_2d<f32>;
@group(0) @binding(2) var lut: texture_2d<f32>;

struct VO {
    @builtin(position) pos: vec4<f32>,
    @location(0) col: vec3<f32>,
    @location(1) shade: f32,
    @location(2) fade: f32,
};

fn h_at(ix: i32, iz: i32) -> f32 {
    let gx = i32(u.p1.x);
    let gz = i32(u.p1.y);
    return textureLoad(heights, vec2<i32>(clamp(ix, 0, gx - 1), clamp(iz, 0, gz - 1)), 0).r;
}

fn norm(db: f32) -> f32 {
    return clamp((db - u.p0.x) / max(u.p0.y - u.p0.x, 1.0), 0.0, 1.0);
}

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VO {
    let gx = u32(u.p1.x);
    let gz = u32(u.p1.y);
    let quad = vi / 6u;
    let corner = vi % 6u;
    let qx = quad % (gx - 1u);
    let qz = quad / (gx - 1u);
    var dx = array<u32, 6>(0u, 1u, 0u, 1u, 1u, 0u);
    var dz = array<u32, 6>(0u, 0u, 1u, 0u, 1u, 1u);
    let ix = i32(qx + dx[corner]);
    let iz = i32(qz + dz[corner]);
    let t = norm(h_at(ix, iz));
    let x = f32(ix) / f32(gx - 1u) * 2.0 - 1.0;
    let z = -f32(iz) / f32(gz - 1u) * u.p2.x;
    let y = t * u.p0.z;
    // normal from the neighbours (central differences)
    let hl = norm(h_at(ix - 1, iz)) * u.p0.z;
    let hr = norm(h_at(ix + 1, iz)) * u.p0.z;
    let hb = norm(h_at(ix, iz - 1)) * u.p0.z;
    let hf = norm(h_at(ix, iz + 1)) * u.p0.z;
    let sx = 4.0 / f32(gx - 1u);
    let sz = 2.0 * u.p2.x / f32(gz - 1u);
    let n = normalize(vec3<f32>(-(hr - hl) / sx, 1.0, (hf - hb) / sz));
    let light = normalize(vec3<f32>(-0.35, 0.85, 0.45));
    var o: VO;
    o.pos = u.mvp * vec4<f32>(x, y, z, 1.0);
    o.col = textureLoad(lut, vec2<i32>(i32(t * 255.0), 0), 0).rgb;
    o.shade = 0.42 + 0.58 * max(dot(n, light), 0.0);
    // older rows fade a little into the background
    o.fade = 1.0 - 0.35 * f32(iz) / f32(gz - 1u);
    return o;
}

@fragment
fn fs(i: VO) -> @location(0) vec4<f32> {
    var c = i.col * i.shade * i.fade;
    if (u.p1.z > 0.5) {
        c = pow(c, vec3<f32>(2.2));
    }
    return vec4<f32>(c, 1.0);
}
"#;

/// Orbit camera around the middle of the landscape.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Camera {
    /// Turn around the vertical axis (radians, 0 = looking from the newest row back in time).
    pub yaw: f32,
    /// Elevation above the floor (radians).
    pub pitch: f32,
    /// Zoom, as a distance: the default frames the whole landscape, larger is further away.
    pub dist: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera { yaw: -0.18, pitch: 0.42, dist: 2.35 }
    }
}

impl Camera {
    /// The view-projection matrix (column-major, wgpu clip space) for a viewport of this aspect. The
    /// landscape is framed to fill the viewport whatever the angle and aspect; `dist` then zooms from
    /// that fit (the default distance is the fit itself, larger is further away).
    pub fn matrix(&self, aspect: f32) -> [f32; 16] {
        // a wide viewport gets a wider landscape (frequency stretched), so it fills the width rather than
        // sitting as a small block in the middle
        let wide = (aspect * 0.5).clamp(1.0, 2.6);
        let eye_d = 2.2 + wide; // eye distance sets the perspective strength, not the size
        let target = [0.0, HEIGHT * 0.25, -DEPTH * 0.5];
        let (cy, sy, cp, sp) = (self.yaw.cos(), self.yaw.sin(), self.pitch.cos(), self.pitch.sin());
        let eye = [target[0] + eye_d * sy * cp, target[1] + eye_d * sp, target[2] + eye_d * cy * cp];
        let mut model = [0.0; 16];
        (model[0], model[5], model[10], model[15]) = (wide, 1.0, 1.0, 1.0);
        let view = mul(&look_at(eye, target, [0.0, 1.0, 0.0]), &model);
        let raw = mul(&perspective(0.70, aspect.max(0.1), 0.05, 50.0), &view);
        // the box's extent on screen, in normalised device coordinates
        let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for i in 0..8 {
            let p = [if i & 1 == 0 { -1.0 } else { 1.0 }, if i & 2 == 0 { 0.0 } else { HEIGHT }, if i & 4 == 0 { 0.0 } else { -DEPTH }];
            let w = raw[3] * p[0] + raw[7] * p[1] + raw[11] * p[2] + raw[15];
            if w <= 1e-4 {
                continue;
            }
            let x = (raw[0] * p[0] + raw[4] * p[1] + raw[8] * p[2] + raw[12]) / w;
            let y = (raw[1] * p[0] + raw[5] * p[1] + raw[9] * p[2] + raw[13]) / w;
            (x0, x1, y0, y1) = (x0.min(x), x1.max(x), y0.min(y), y1.max(y));
        }
        if x0 > x1 {
            return raw;
        }
        // scale and shift in clip space so the box fills the viewport, leaving room for the labels
        let zoom = Camera::default().dist / self.dist.max(0.1);
        let s = (1.74 / (x1 - x0).max(1e-3)).min(1.66 / (y1 - y0).max(1e-3)) * zoom;
        let (cx, cyc) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5 + 0.04);
        let mut fit = [0.0; 16];
        fit[0] = s;
        fit[5] = s;
        fit[10] = 1.0;
        fit[15] = 1.0;
        fit[12] = -s * cx;
        fit[13] = -s * cyc;
        mul(&fit, &raw)
    }

    /// Where a world point lands in `rect`, if it's in front of the camera.
    pub fn project(m: &[f32; 16], rect: Rect, p: [f32; 3]) -> Option<Pos2> {
        let x = m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12];
        let y = m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13];
        let w = m[3] * p[0] + m[7] * p[1] + m[11] * p[2] + m[15];
        if w <= 1e-4 {
            return None;
        }
        Some(pos2(rect.left() + (x / w + 1.0) * 0.5 * rect.width(), rect.top() + (1.0 - y / w) * 0.5 * rect.height()))
    }
}

fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fovy / 2.0).tan();
    let mut m = [0.0; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = far / (near - far);
    m[11] = -1.0;
    m[14] = near * far / (near - far);
    m
}

fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let normal = |a: [f32; 3]| {
        let l = dot(a, a).sqrt().max(1e-9);
        [a[0] / l, a[1] / l, a[2] / l]
    };
    let f = normal(sub(target, eye));
    let s = normal(cross(f, up));
    let u = cross(s, f);
    [s[0], u[0], -f[0], 0.0, s[1], u[1], -f[1], 0.0, s[2], u[2], -f[2], 0.0, -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0]
}

fn mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut m = [0.0; 16];
    for c in 0..4 {
        for r in 0..4 {
            m[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    m
}

/// GPU resources, kept in egui-wgpu's callback resources.
struct Res {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    lut: wgpu::Texture,
    lut_view: wgpu::TextureView,
    heights: Option<(wgpu::Texture, [u32; 2])>,
    bind: Option<wgpu::BindGroup>,
    linear: bool,
}

/// Set up the pipeline (once, at startup). Returns false when the renderer has no depth buffer.
pub fn init(rs: &egui_wgpu::RenderState, depth: Option<wgpu::TextureFormat>) -> bool {
    let Some(depth) = depth else { return false };
    let device = &rs.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("waterfall3d"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Texture { multisampled: false, sample_type: wgpu::TextureSampleType::Float { filterable: false }, view_dimension: wgpu::TextureViewDimension::D2 },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("waterfall3d"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            },
            tex_entry(1),
            tex_entry(2),
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("waterfall3d"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("waterfall3d"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), buffers: &[], compilation_options: wgpu::PipelineCompilationOptions::default() },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::default(),
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::default(),
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: depth,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState { count: MSAA, mask: !0, alpha_to_coverage_enabled: false },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState { format: rs.target_format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview_mask: None,
        cache: None,
    });
    let uniform = device.create_buffer(&wgpu::BufferDescriptor { label: Some("waterfall3d uniforms"), size: 32 * 4, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    let lut = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("waterfall3d palette"),
        size: wgpu::Extent3d { width: 256, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let lut_view = lut.create_view(&wgpu::TextureViewDescriptor::default());
    let linear = rs.target_format.is_srgb();
    rs.renderer.write().callback_resources.insert(Res { pipeline, layout, uniform, lut, lut_view, heights: None, bind: None, linear });
    true
}

/// One frame of the 3D view: the matrix and levels, plus new heights or a new palette when they changed.
pub struct Callback {
    pub mvp: [f32; 16],
    pub lo: f32,
    pub hi: f32,
    pub gx: usize,
    pub gz: usize,
    pub heights: Option<Arc<Vec<f32>>>,
    pub lut: Option<Arc<Vec<[u8; 4]>>>,
}

fn bytes<T>(v: &[T]) -> &[u8] {
    // SAFETY: plain f32/u8 data, read as bytes for upload.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

impl egui_wgpu::CallbackTrait for Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = resources.get_mut::<Res>() else { return Vec::new() };
        if self.gx < 2 || self.gz < 2 {
            return Vec::new();
        }
        if let Some(lut) = &self.lut {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &res.lut, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                bytes(lut),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256 * 4), rows_per_image: Some(1) },
                wgpu::Extent3d { width: 256, height: 1, depth_or_array_layers: 1 },
            );
        }
        let size = [self.gx as u32, self.gz as u32];
        if res.heights.as_ref().is_none_or(|(_, s)| *s != size) {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("waterfall3d heights"),
                size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            res.bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("waterfall3d"),
                layout: &res.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: res.uniform.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&res.lut_view) },
                ],
            }));
            res.heights = Some((tex, size));
        }
        if let (Some(h), Some((tex, _))) = (&self.heights, &res.heights)
            && h.len() == self.gx * self.gz
        {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                bytes(h),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size[0] * 4), rows_per_image: Some(size[1]) },
                wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            );
        }
        let mut u = [0f32; 32];
        u[..16].copy_from_slice(&self.mvp);
        u[16..20].copy_from_slice(&[self.lo, self.hi, HEIGHT, 0.0]);
        u[20..24].copy_from_slice(&[self.gx as f32, self.gz as f32, if res.linear { 1.0 } else { 0.0 }, 0.0]);
        u[24] = DEPTH;
        queue.write_buffer(&res.uniform, 0, bytes(&u));
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(res) = resources.get::<Res>() else { return };
        let Some(bind) = &res.bind else { return };
        if self.gx < 2 || self.gz < 2 {
            return;
        }
        pass.set_pipeline(&res.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..((self.gx - 1) * (self.gz - 1) * 6) as u32, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_keeps_levels_and_narrow_peaks() {
        let (gx, gz) = (64, 16);
        // a flat field stays flat
        let mut h = vec![-90.0f32; gx * gz];
        smooth(&mut h, gx, gz, 1.2, 1.2);
        assert!(h.iter().all(|v| (v + 90.0).abs() < 0.01));
        // a one-cell-wide carrier 40 dB up, present in every row: stays put and keeps most of its height
        let mut h = vec![-90.0f32; gx * gz];
        for z in 0..gz {
            h[z * gx + 30] = -50.0;
        }
        smooth(&mut h, gx, gz, 1.2, 1.2);
        let row = &h[8 * gx..9 * gx];
        let peak = row.iter().cloned().fold(f32::MIN, f32::max);
        assert_eq!(row.iter().position(|&v| v == peak), Some(30));
        assert!(peak > -50.0 - 6.0, "{peak}");
        assert!(row[20] < -89.0, "{}", row[20]);
    }

    #[test]
    fn the_landscape_fills_the_view() {
        let c = Camera::default();
        let m = c.matrix(2.0);
        let r = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for i in 0..8 {
            let p = [if i & 1 == 0 { -1.0 } else { 1.0 }, if i & 2 == 0 { 0.0 } else { HEIGHT }, if i & 4 == 0 { 0.0 } else { -DEPTH }];
            let q = Camera::project(&m, r, p).unwrap();
            (x0, x1, y0, y1) = (x0.min(q.x), x1.max(q.x), y0.min(q.y), y1.max(q.y));
        }
        // inside the rect, and filling it in at least one direction
        assert!(x0 >= 0.0 && x1 <= 200.0 && y0 >= 0.0 && y1 <= 100.0, "{x0} {x1} {y0} {y1}");
        assert!(x1 - x0 > 160.0 || y1 - y0 > 75.0, "{x0} {x1} {y0} {y1}");
        // the newest row (z = 0) is nearer the viewer than the oldest, so lower on screen
        let front = Camera::project(&m, r, [0.0, 0.0, 0.0]).unwrap();
        let back = Camera::project(&m, r, [0.0, 0.0, -DEPTH]).unwrap();
        assert!(front.y > back.y);
        // frequency runs left to right from the front
        let lo = Camera::project(&m, r, [-1.0, 0.0, 0.0]).unwrap();
        let hi = Camera::project(&m, r, [1.0, 0.0, 0.0]).unwrap();
        assert!(hi.x > lo.x);
    }
}
