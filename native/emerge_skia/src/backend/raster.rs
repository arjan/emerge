//! Raster (offscreen CPU) backend.
//!
//! This backend renders to a CPU-backed surface without any windowing.
//! Useful for testing, headless rendering, and generating images.

use skia_safe::{AlphaType, ColorType, ImageInfo, surfaces};

use crate::render_damage::{Damage, DamageRect, SceneDamageState};
use crate::renderer::{
    RenderFrame, RenderState, RenderTimings, RendererCacheConfig, SceneRenderer, text_surface_props,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RasterPixelFormat {
    #[default]
    Rgba8888Premul,
    /// B, G, R, A byte order; matches little-endian DRM ARGB8888/XRGB8888.
    Bgra8888Premul,
    Gray8Opaque,
}

impl RasterPixelFormat {
    pub fn color_type(self) -> ColorType {
        match self {
            Self::Rgba8888Premul => ColorType::RGBA8888,
            Self::Bgra8888Premul => ColorType::BGRA8888,
            Self::Gray8Opaque => ColorType::Gray8,
        }
    }

    pub fn alpha_type(self) -> AlphaType {
        match self {
            Self::Rgba8888Premul | Self::Bgra8888Premul => AlphaType::Premul,
            Self::Gray8Opaque => AlphaType::Opaque,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8888Premul | Self::Bgra8888Premul => 4,
            Self::Gray8Opaque => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RasterConfig {
    pub width: u32,
    pub height: u32,
}

impl Default for RasterConfig {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
        }
    }
}

#[derive(Clone)]
pub struct RasterFrame {
    pub data: Vec<u8>,
    pub row_bytes: usize,
    pub format: RasterPixelFormat,
}

pub struct RasterBackend {
    renderer: SceneRenderer,
    surface: skia_safe::Surface,
    width: u32,
    height: u32,
    format: RasterPixelFormat,
    /// Scene drawn into `mirror` by the last `render_damaged`; `None` forces a
    /// full repaint.
    damage_state: Option<SceneDamageState>,
    /// Copy of the surface pixels, refreshed only where frames change.
    mirror: Vec<u8>,
}

impl RasterBackend {
    pub fn new(config: &RasterConfig) -> Result<Self, String> {
        Self::with_cache_config(config, RendererCacheConfig::default())
    }

    pub fn with_cache_config(
        config: &RasterConfig,
        cache_config: RendererCacheConfig,
    ) -> Result<Self, String> {
        Self::with_cache_config_and_format(config, cache_config, RasterPixelFormat::default())
    }

    pub fn with_cache_config_and_format(
        config: &RasterConfig,
        cache_config: RendererCacheConfig,
        format: RasterPixelFormat,
    ) -> Result<Self, String> {
        let surface = create_surface(config.width, config.height, format)?;

        Ok(Self {
            renderer: SceneRenderer::with_cache_config(cache_config),
            surface,
            width: config.width,
            height: config.height,
            format,
            damage_state: None,
            mirror: Vec::new(),
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.width == width && self.height == height {
            return Ok(());
        }

        self.surface = create_surface(width, height, self.format)?;
        self.renderer.invalidate_visible_frame_fingerprint();
        self.width = width;
        self.height = height;
        self.damage_state = None;
        Ok(())
    }

    pub fn sync_cpu_video_frames(
        &mut self,
        registry: &std::sync::Arc<crate::video::VideoRegistry>,
    ) -> Result<bool, String> {
        self.renderer.sync_cpu_video_frames(registry)
    }

    pub fn render(&mut self, state: &RenderState) -> RasterFrame {
        self.render_with_timings(state).0
    }

    pub fn render_with_timings(&mut self, state: &RenderState) -> (RasterFrame, RenderTimings) {
        let mut frame = RenderFrame::new(&mut self.surface, None);
        let timings = self.renderer.render(&mut frame, state);
        // The mirror no longer matches what a partial repaint would build on.
        self.damage_state = None;
        let row_bytes = self.row_bytes();
        let byte_len = row_bytes
            .checked_mul(self.height as usize)
            .expect("validated raster surface dimensions must fit memory");
        let data = self.copy_pixels(row_bytes, byte_len);

        (
            RasterFrame {
                data,
                row_bytes,
                format: self.format,
            },
            timings,
        )
    }

    pub fn row_bytes(&self) -> usize {
        usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(self.format.bytes_per_pixel()))
            .expect("validated raster surface width must fit memory")
    }

    pub fn format(&self) -> RasterPixelFormat {
        self.format
    }

    /// Repaints only what changed since the previous `render_damaged` and
    /// refreshes those pixels in `pixels()`.
    pub fn render_damaged(&mut self, state: &RenderState) -> (Damage, RenderTimings) {
        let scene_state = {
            let _fonts = state.scene.fonts.as_ref().map(|fonts| fonts.enter());
            SceneDamageState::capture(
                &state.scene,
                u64::from_be_bytes([
                    0,
                    0,
                    0,
                    0,
                    state.clear_color.a(),
                    state.clear_color.r(),
                    state.clear_color.g(),
                    state.clear_color.b(),
                ]),
                self.width,
                self.height,
            )
        };
        let row_bytes = self.row_bytes();
        let byte_len = row_bytes * self.height as usize;
        // The cache-tracking path can reuse layer content from outside the
        // damage, so it always repaints in full.
        let damage = if self.mirror.len() == byte_len && !self.renderer.tracks_cache(state) {
            scene_state.damage_since(self.damage_state.as_ref())
        } else {
            Damage::Full
        };

        let mut frame = RenderFrame::new(&mut self.surface, None);
        let timings = match &damage {
            Damage::None => RenderTimings::default(),
            Damage::Rects(rects) => self.renderer.render_within(&mut frame, state, rects),
            Damage::Full => self.renderer.render(&mut frame, state),
        };
        self.damage_state = Some(scene_state);

        match damage.bounds(self.width, self.height) {
            Some(rect) if self.mirror.len() == byte_len => self.copy_region(rect, row_bytes),
            Some(_) => self.mirror = self.copy_pixels(row_bytes, byte_len),
            None => {}
        }
        (damage, timings)
    }

    /// Pixels as of the last `render_damaged`, `row_bytes()` per row.
    pub fn pixels(&self) -> &[u8] {
        &self.mirror
    }

    fn copy_region(&mut self, rect: DamageRect, row_bytes: usize) {
        let bpp = self.format.bytes_per_pixel();
        let (start, len) = (rect.x as usize * bpp, rect.width as usize * bpp);
        let rows = rect.y as usize..(rect.y + rect.height) as usize;
        let mirror = &mut self.mirror;
        let copied = self.surface.peek_pixels().and_then(|pixmap| {
            let bytes = (pixmap.row_bytes() == row_bytes)
                .then(|| pixmap.bytes())
                .flatten()?;
            rows.clone().for_each(|y| {
                let offset = y * row_bytes + start;
                mirror[offset..offset + len].copy_from_slice(&bytes[offset..offset + len]);
            });
            Some(())
        });
        if copied.is_none() {
            let byte_len = self.mirror.len();
            self.mirror = self.copy_pixels(row_bytes, byte_len);
        }
    }

    /// Copies the surface pixels in one pass. A raster surface already stores
    /// pixels in the requested format, so borrowing them avoids zero-filling a
    /// buffer and running Skia's general readback.
    fn copy_pixels(&mut self, row_bytes: usize, byte_len: usize) -> Vec<u8> {
        let peeked = self.surface.peek_pixels().and_then(|pixmap| {
            (pixmap.row_bytes() == row_bytes)
                .then(|| pixmap.bytes())
                .flatten()
                .and_then(|bytes| bytes.get(..byte_len).map(<[u8]>::to_vec))
        });

        peeked.unwrap_or_else(|| {
            let mut data = vec![0_u8; byte_len];
            let info = ImageInfo::new(
                (self.width as i32, self.height as i32),
                self.format.color_type(),
                self.format.alpha_type(),
                None,
            );
            let read = self
                .surface
                .read_pixels(&info, &mut data, row_bytes, (0, 0));
            debug_assert!(read, "raster surface readback must match its native format");
            data
        })
    }

    pub fn render_grayscale_dither_policy(&self, state: &RenderState) -> Result<Vec<u8>, String> {
        self.renderer
            .render_grayscale_dither_policy(self.width, self.height, state)
    }
}

fn create_surface(
    width: u32,
    height: u32,
    format: RasterPixelFormat,
) -> Result<skia_safe::Surface, String> {
    let width_i32 = i32::try_from(width).map_err(|_| "raster width is too large".to_string())?;
    let height_i32 = i32::try_from(height).map_err(|_| "raster height is too large".to_string())?;
    let info = ImageInfo::new(
        (width_i32, height_i32),
        format.color_type(),
        format.alpha_type(),
        None,
    );

    let surface = match format {
        RasterPixelFormat::Rgba8888Premul | RasterPixelFormat::Bgra8888Premul => {
            let surface_props = text_surface_props();
            surfaces::raster(&info, None, Some(&surface_props))
        }
        RasterPixelFormat::Gray8Opaque => surfaces::raster(&info, None, None),
    };
    surface.ok_or_else(|| format!("failed to create {format:?} raster surface"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{render_scene::RenderScene, renderer::RenderState};
    use skia_safe::Color;

    mod damage {
        use super::*;
        use crate::render_damage::Damage;
        use crate::render_scene::{DrawPrimitive, RenderNode};
        use crate::tree::geometry::{ClipShape, Rect};
        use crate::tree::transform::Affine2;

        const W: u32 = 200;
        const H: u32 = 160;

        #[derive(Clone)]
        struct Row {
            y: f32,
            color: u32,
            label: &'static str,
        }

        #[derive(Clone)]
        struct Spec {
            rows: Vec<Row>,
            shadow_offset: f32,
            clip_width: f32,
            alpha: f32,
            spin: f32,
            front_first: bool,
        }

        fn base() -> Spec {
            Spec {
                rows: vec![
                    Row {
                        y: 10.0,
                        color: 0x3366CCFF,
                        label: "alpha",
                    },
                    Row {
                        y: 40.0,
                        color: 0xCC3333FF,
                        label: "beta",
                    },
                    Row {
                        y: 70.0,
                        color: 0x33AA55FF,
                        label: "gamma",
                    },
                ],
                shadow_offset: 3.0,
                clip_width: 60.0,
                alpha: 0.5,
                spin: 0.0,
                front_first: true,
            }
        }

        fn text(x: f32, y: f32, label: &str) -> RenderNode {
            RenderNode::Primitive(DrawPrimitive::TextWithFont(
                x,
                y,
                label.to_string(),
                14.0,
                0xFFFFFFFFu32.into(),
                "default".to_string(),
                400,
                false,
            ))
        }

        fn scene(spec: &Spec) -> RenderScene {
            let rows = spec.rows.iter().flat_map(|row| {
                [
                    RenderNode::Primitive(DrawPrimitive::RoundedRect(
                        8.0,
                        row.y,
                        120.0,
                        24.0,
                        6.0,
                        row.color.into(),
                    )),
                    text(14.0, row.y + 17.0, row.label),
                ]
            });
            let overlap = [
                RenderNode::Primitive(DrawPrimitive::Rect(
                    140.0,
                    10.0,
                    40.0,
                    40.0,
                    0xFF8800FFu32.into(),
                )),
                RenderNode::Primitive(DrawPrimitive::Rect(
                    150.0,
                    20.0,
                    40.0,
                    40.0,
                    0x0088FFFFu32.into(),
                )),
            ];
            let overlap: Vec<RenderNode> = if spec.front_first {
                overlap.to_vec()
            } else {
                overlap.into_iter().rev().collect()
            };
            let nodes = std::iter::once(RenderNode::Primitive(DrawPrimitive::Rect(
                0.0,
                0.0,
                W as f32,
                H as f32,
                0x202020FFu32.into(),
            )))
            .chain(rows)
            .chain(overlap)
            .chain([
                RenderNode::ShadowPass {
                    children: vec![RenderNode::Primitive(DrawPrimitive::Shadow(
                        140.0,
                        100.0,
                        30.0,
                        30.0,
                        spec.shadow_offset,
                        spec.shadow_offset,
                        4.0,
                        0.0,
                        [4.0; 4],
                        0x000000AAu32.into(),
                    ))],
                },
                RenderNode::Clip {
                    clips: vec![ClipShape {
                        rect: Rect {
                            x: 10.0,
                            y: 110.0,
                            width: spec.clip_width,
                            height: 30.0,
                        },
                        radii: None,
                    }],
                    children: vec![RenderNode::Primitive(DrawPrimitive::Rect(
                        0.0,
                        100.0,
                        120.0,
                        60.0,
                        0xAAAA00FFu32.into(),
                    ))],
                },
                RenderNode::Alpha {
                    alpha: spec.alpha,
                    children: vec![RenderNode::Primitive(DrawPrimitive::Rect(
                        90.0,
                        120.0,
                        30.0,
                        30.0,
                        0xFF00FFFFu32.into(),
                    ))],
                },
                RenderNode::Transform {
                    transform: Affine2::translation(170.0, 140.0)
                        .then(Affine2::scale(1.0 + spec.spin, 1.0)),
                    children: vec![RenderNode::Primitive(DrawPrimitive::Rect(
                        -5.0,
                        -5.0,
                        10.0,
                        10.0,
                        0xFFFFFFFFu32.into(),
                    ))],
                },
            ])
            .collect();
            RenderScene {
                nodes,
                ..RenderScene::default()
            }
        }

        fn backend() -> RasterBackend {
            RasterBackend::with_cache_config_and_format(
                &RasterConfig {
                    width: W,
                    height: H,
                },
                RendererCacheConfig {
                    enabled: false,
                    ..RendererCacheConfig::default()
                },
                RasterPixelFormat::Rgba8888Premul,
            )
            .unwrap()
        }

        fn state(spec: &Spec, clear: Color) -> RenderState {
            RenderState::new(scene(spec), clear, 1, false)
        }

        /// Applies each edit incrementally and checks the result against a
        /// from-scratch render of the same scene.
        fn assert_matches_full_render(edits: Vec<(&str, Spec, Color)>) -> Vec<Damage> {
            let mut incremental = backend();
            incremental.render_damaged(&state(&base(), Color::TRANSPARENT));
            edits
                .into_iter()
                .map(|(name, spec, clear)| {
                    let (damage, _) = incremental.render_damaged(&state(&spec, clear));
                    let full = backend().render(&state(&spec, clear));
                    assert!(
                        incremental.pixels() == full.data.as_slice(),
                        "partial repaint differs from full render after {name} ({damage:?})"
                    );
                    damage
                })
                .collect()
        }

        fn edit(change: impl FnOnce(&mut Spec)) -> Spec {
            let mut spec = base();
            change(&mut spec);
            spec
        }

        #[test]
        fn partial_repaints_match_full_renders() {
            let clear = Color::TRANSPARENT;
            let damages = assert_matches_full_render(vec![
                ("text change", edit(|s| s.rows[1].label = "BETA!"), clear),
                (
                    "colour change",
                    edit(|s| s.rows[2].color = 0x8844CCFF),
                    clear,
                ),
                ("move", edit(|s| s.rows[0].y = 15.0), clear),
                (
                    "remove",
                    edit(|s| {
                        s.rows.remove(1);
                    }),
                    clear,
                ),
                (
                    "insert",
                    edit(|s| {
                        s.rows.push(Row {
                            y: 100.0,
                            color: 0x555555FF,
                            label: "new",
                        })
                    }),
                    clear,
                ),
                ("reorder overlap", edit(|s| s.front_first = false), clear),
                ("shadow offset", edit(|s| s.shadow_offset = 8.0), clear),
                ("clip change", edit(|s| s.clip_width = 90.0), clear),
                ("alpha change", edit(|s| s.alpha = 0.9), clear),
                ("transform change", edit(|s| s.spin = 0.5), clear),
                ("unchanged", edit(|s| s.spin = 0.5), clear),
                (
                    "clear colour",
                    edit(|s| s.spin = 0.5),
                    Color::from_rgb(1, 2, 3),
                ),
                ("back to base", base(), clear),
            ]);

            assert!(
                matches!(damages[0], Damage::Rects(_)),
                "text change: {:?}",
                damages[0]
            );
            assert!(
                matches!(damages[5], Damage::Full),
                "reorder: {:?}",
                damages[5]
            );
            assert_eq!(damages[10], Damage::None);
            assert_eq!(damages[11], Damage::Full);
        }

        #[test]
        fn small_changes_damage_small_areas() {
            let mut backend = backend();
            backend.render_damaged(&state(&base(), Color::TRANSPARENT));
            let (damage, _) = backend.render_damaged(&state(
                &edit(|s| s.rows[1].label = "BETA!"),
                Color::TRANSPARENT,
            ));
            let bounds = damage.bounds(W, H).unwrap();
            assert!(bounds.height < 30 && bounds.y >= 35, "{bounds:?}");
        }
    }

    #[test]
    fn gray8_surface_returns_one_byte_per_pixel() {
        let mut backend = RasterBackend::with_cache_config_and_format(
            &RasterConfig {
                width: 3,
                height: 2,
            },
            RendererCacheConfig {
                enabled: false,
                ..RendererCacheConfig::default()
            },
            RasterPixelFormat::Gray8Opaque,
        )
        .unwrap();
        let state = RenderState::new(RenderScene::default(), Color::WHITE, 1, false);
        let frame = backend.render(&state);

        assert_eq!(frame.format, RasterPixelFormat::Gray8Opaque);
        assert_eq!(frame.row_bytes, 3);
        assert_eq!(frame.data, vec![255; 6]);
    }

    #[test]
    fn rgba_and_bgra_surfaces_store_their_byte_order() {
        let render = |format| {
            let mut backend = RasterBackend::with_cache_config_and_format(
                &RasterConfig {
                    width: 2,
                    height: 1,
                },
                RendererCacheConfig {
                    enabled: false,
                    ..RendererCacheConfig::default()
                },
                format,
            )
            .unwrap();
            let state = RenderState::new(
                RenderScene::default(),
                Color::from_rgb(10, 20, 30),
                1,
                false,
            );
            backend.render(&state)
        };

        let rgba = render(RasterPixelFormat::Rgba8888Premul);
        let bgra = render(RasterPixelFormat::Bgra8888Premul);

        assert_eq!(rgba.row_bytes, 8);
        assert_eq!(rgba.data, [10, 20, 30, 255].repeat(2));
        assert_eq!(bgra.format, RasterPixelFormat::Bgra8888Premul);
        assert_eq!(bgra.row_bytes, 8);
        assert_eq!(bgra.data, [30, 20, 10, 255].repeat(2));
    }
}
