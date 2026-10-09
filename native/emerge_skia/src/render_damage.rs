//! Damage tracking between consecutive render scenes.
//!
//! A scene is flattened into draw items: each primitive's device-space bounds
//! plus a key that hashes the primitive together with every transform, clip,
//! alpha, shadow pass and paint layer it is drawn under. Items whose key only
//! appears in one of two scenes changed, so their old and new bounds must be
//! repainted. Anything this model cannot see falls back to full damage.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::render_scene::{
    DrawPrimitive, PaintLayerHashFloat, RenderNode, RenderPaintLayerContentNode, RenderScene,
    draw_primitive_visual_bounds, hash_paint_layer_affine2, hash_paint_layer_clip_shapes,
    hash_paint_layer_draw_primitive, hash_paint_layer_rect,
};
use crate::tree::geometry::{ClipShape, Rect};
use crate::tree::transform::Affine2;

/// Extra pixels around every item, covering antialiasing and subpixel text.
const DAMAGE_OUTSET: i32 = 2;
/// Above this many disjoint rectangles the damage collapses to their bounds.
const MAX_DAMAGE_RECTS: usize = 8;

const FLOAT: PaintLayerHashFloat = PaintLayerHashFloat::Exact;

/// Integer device-space rectangle, half-open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl DamageRect {
    const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width: width as i32,
            height: height as i32,
        }
    }

    fn right(self) -> i32 {
        self.x + self.width
    }

    fn bottom(self) -> i32 {
        self.y + self.height
    }

    pub fn is_empty(self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    fn contains(self, other: Self) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && self.right() >= other.right()
            && self.bottom() >= other.bottom()
    }

    /// True when `self` is partly inside `rect`, so a clip to `rect` would cut it.
    fn crosses_edge_of(self, rect: Self) -> bool {
        self.overlaps(rect) && !rect.contains(self) && !self.contains(rect)
    }

    fn touches(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    fn clamp_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.max(0);
        let y = self.y.max(0);
        let right = self.right().min(width as i32);
        let bottom = self.bottom().min(height as i32);
        let rect = Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        };
        (!rect.is_empty()).then_some(rect)
    }

    fn from_bounds(bounds: Rect) -> Option<Self> {
        if !(bounds.x.is_finite()
            && bounds.y.is_finite()
            && bounds.width.is_finite()
            && bounds.height.is_finite())
        {
            return None;
        }
        let x = bounds.x.floor() as i32 - DAMAGE_OUTSET;
        let y = bounds.y.floor() as i32 - DAMAGE_OUTSET;
        let right = (bounds.x + bounds.width).ceil() as i32 + DAMAGE_OUTSET;
        let bottom = (bounds.y + bounds.height).ceil() as i32 + DAMAGE_OUTSET;
        Some(Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

/// What changed between two frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Nothing visible changed.
    None,
    /// Only these disjoint rectangles changed.
    Rects(Vec<DamageRect>),
    /// Repaint everything.
    Full,
}

impl Damage {
    /// Bounding box of the damage, if any.
    pub fn bounds(&self, width: u32, height: u32) -> Option<DamageRect> {
        match self {
            Self::None => None,
            Self::Rects(rects) => rects.iter().copied().reduce(DamageRect::union),
            Self::Full => Some(DamageRect::full(width, height)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DrawItem {
    key: u64,
    /// Visible device pixels; empty when clipped away or off the surface,
    /// `None` when the bounds are unknown.
    bounds: Option<DamageRect>,
    /// Has partially covered edge pixels, which a clip edge can change.
    antialiased: bool,
}

/// Flattened scene kept between frames.
#[derive(Clone, Default)]
pub struct SceneDamageState {
    items: Vec<DrawItem>,
    /// Clear colour and surface size.
    globals: u64,
    fonts: Option<crate::renderer::FontSnapshot>,
    images: Option<crate::renderer::ImageSnapshot>,
    /// The scene contains something that changes without a scene change.
    volatile: bool,
}

impl SceneDamageState {
    /// Flattens `scene`. Text bounds need the scene's fonts entered by the caller.
    pub fn capture(scene: &RenderScene, clear_key: u64, width: u32, height: u32) -> Self {
        let mut flattener = Flattener {
            items: Vec::new(),
            volatile: false,
        };
        flattener.nodes(&scene.nodes, Context::root());

        let mut hasher = DefaultHasher::new();
        clear_key.hash(&mut hasher);
        (width, height).hash(&mut hasher);

        Self {
            items: flattener
                .items
                .into_iter()
                .map(|item| DrawItem {
                    bounds: item
                        .bounds
                        .map(|rect| rect.clamp_to(width, height).unwrap_or(DamageRect::EMPTY)),
                    ..item
                })
                .collect(),
            globals: hasher.finish(),
            fonts: scene.fonts.clone(),
            images: scene.images.clone(),
            volatile: flattener.volatile,
        }
    }

    /// Damage from `previous` to `self`.
    pub fn damage_since(&self, previous: Option<&Self>) -> Damage {
        let Some(previous) = previous else {
            return Damage::Full;
        };
        if self.volatile
            || previous.volatile
            || self.globals != previous.globals
            || self.fonts != previous.fonts
            || self.images != previous.images
        {
            return Damage::Full;
        }

        let counts = |items: &[DrawItem]| {
            items.iter().fold(HashMap::new(), |mut counts, item| {
                *counts.entry(item.key).or_insert(0_usize) += 1;
                counts
            })
        };
        let old_counts = counts(&previous.items);
        let new_counts = counts(&self.items);

        // Keep the first `n` occurrences of each key, where `n` is how often it
        // appears in the other scene. The rest are added or removed items.
        let split = |items: &[DrawItem], other: &HashMap<u64, usize>| {
            let mut seen: HashMap<u64, usize> = HashMap::new();
            items
                .iter()
                .fold((Vec::new(), Vec::new()), |(mut kept, mut changed), item| {
                    let count = seen.entry(item.key).or_insert(0);
                    *count += 1;
                    if *count <= other.get(&item.key).copied().unwrap_or(0) {
                        kept.push(item.key);
                    } else {
                        changed.push(*item);
                    }
                    (kept, changed)
                })
        };
        let (old_kept, removed) = split(&previous.items, &new_counts);
        let (new_kept, added) = split(&self.items, &old_counts);

        // Unchanged items drawn in a different order can paint differently.
        if old_kept != new_kept {
            return Damage::Full;
        }

        let changed: Vec<DamageRect> = match removed
            .iter()
            .chain(added.iter())
            .map(|item| item.bounds)
            .collect::<Option<Vec<_>>>()
        {
            Some(rects) => rects.into_iter().filter(|rect| !rect.is_empty()).collect(),
            None => return Damage::Full,
        };

        if changed.is_empty() {
            return Damage::None;
        }
        match self.expand_to_whole_items(merge_rects(changed)) {
            Some(rects) => Damage::Rects(rects),
            None => Damage::Full,
        }
    }

    /// Grows `rects` until no item's edge crosses a rectangle edge. Skia's
    /// antialiasing coverage for a shape changes slightly when a clip edge cuts
    /// through it, so partial repaints only match full renders when every
    /// repainted shape is either wholly inside or wholly covering the clip.
    fn expand_to_whole_items(&self, rects: Vec<DamageRect>) -> Option<Vec<DamageRect>> {
        const MAX_ROUNDS: usize = 16;
        let bounds: Vec<DamageRect> = self
            .items
            .iter()
            .filter(|item| item.antialiased)
            .filter_map(|item| item.bounds)
            .filter(|rect| !rect.is_empty())
            .collect();

        (0..MAX_ROUNDS)
            .try_fold(rects, |rects, _| {
                let grown: Vec<DamageRect> = rects
                    .iter()
                    .map(|rect| {
                        bounds
                            .iter()
                            .filter(|item| item.crosses_edge_of(*rect))
                            .fold(*rect, |acc, item| acc.union(*item))
                    })
                    .collect();
                if grown == rects {
                    Err(rects)
                } else {
                    Ok(merge_rects(grown))
                }
            })
            .err()
    }
}

/// Merges touching rectangles until they are disjoint, then caps the count.
fn merge_rects(rects: Vec<DamageRect>) -> Vec<DamageRect> {
    let merged = rects
        .into_iter()
        .fold(Vec::<DamageRect>::new(), |acc, rect| {
            let (touching, rest): (Vec<_>, Vec<_>) = acc.into_iter().partition(|r| r.touches(rect));
            let joined = touching.into_iter().fold(rect, DamageRect::union);
            rest.into_iter().chain(std::iter::once(joined)).collect()
        });

    // A union can newly touch an earlier rectangle; repeat until stable.
    let stable = if merged
        .iter()
        .enumerate()
        .any(|(i, a)| merged.iter().skip(i + 1).any(|b| a.touches(*b)))
    {
        merge_rects(merged)
    } else {
        merged
    };

    if stable.len() > MAX_DAMAGE_RECTS {
        stable
            .into_iter()
            .reduce(DamageRect::union)
            .into_iter()
            .collect()
    } else {
        stable
    }
}

#[derive(Clone, Copy)]
struct Context {
    /// Hash of the transform/clip/alpha/layer chain above this point.
    key: u64,
    transform: Affine2,
    /// Device-space clip bounds, if any clip applies.
    clip: Option<Rect>,
    /// A rounded or fractional clip applies, so edges are antialiased.
    rounded_clip: bool,
}

impl Context {
    fn root() -> Self {
        Self {
            key: 0,
            transform: Affine2::identity(),
            clip: None,
            rounded_clip: false,
        }
    }

    fn child(self, hash: impl FnOnce(&mut DefaultHasher)) -> Self {
        let mut hasher = DefaultHasher::new();
        self.key.hash(&mut hasher);
        hash(&mut hasher);
        Self {
            key: hasher.finish(),
            ..self
        }
    }

    fn clipped(self, clips: &[ClipShape], tag: u8) -> Self {
        let clip_bounds = clips
            .iter()
            .map(|clip| self.transform.map_rect_aabb(clip.rect))
            .reduce(|a, b| a.intersect(b).unwrap_or_default());
        let next = self.child(|hasher| {
            tag.hash(hasher);
            hash_paint_layer_clip_shapes(hasher, clips, FLOAT);
        });
        Context {
            clip: match (self.clip, clip_bounds) {
                (Some(outer), Some(inner)) => Some(outer.intersect(inner).unwrap_or_default()),
                (outer, inner) => inner.or(outer),
            },
            rounded_clip: self.rounded_clip
                || clips.iter().any(|clip| clip.radii.is_some())
                || !is_pixel_aligned(&self.transform, clips.iter().map(|clip| clip.rect)),
            ..next
        }
    }
}

struct Flattener {
    items: Vec<DrawItem>,
    volatile: bool,
}

impl Flattener {
    fn nodes(&mut self, nodes: &[RenderNode], context: Context) {
        nodes.iter().for_each(|node| self.node(node, context));
    }

    fn node(&mut self, node: &RenderNode, context: Context) {
        match node {
            // Shadow passes draw outside the clip that contains them.
            RenderNode::ShadowPass { children } => self.nodes(
                children,
                Context {
                    clip: None,
                    ..context.child(|hasher| 1u8.hash(hasher))
                },
            ),
            RenderNode::Clip { clips, children } => self.nodes(children, context.clipped(clips, 2)),
            // A relaxed clip may let content escape; keep its identity in the
            // key but not its bounds.
            RenderNode::RelaxedClip { clips, children } => self.nodes(
                children,
                context.child(|hasher| {
                    3u8.hash(hasher);
                    hash_paint_layer_clip_shapes(hasher, clips, FLOAT);
                }),
            ),
            RenderNode::Transform {
                transform,
                children,
            } => self.nodes(
                children,
                Context {
                    transform: context.transform.then(*transform),
                    ..context.child(|hasher| {
                        4u8.hash(hasher);
                        hash_paint_layer_affine2(hasher, *transform, FLOAT);
                    })
                },
            ),
            RenderNode::Alpha { alpha, children } => self.nodes(
                children,
                context.child(|hasher| {
                    5u8.hash(hasher);
                    alpha.to_bits().hash(hasher);
                }),
            ),
            RenderNode::PaintLayer(layer) => {
                let layer_context = context.child(|hasher| {
                    6u8.hash(hasher);
                    layer.id.hash(hasher);
                    hash_paint_layer_rect(hasher, layer.bounds, FLOAT);
                });
                self.layer_content(&layer.content.nodes, layer_context);
            }
            RenderNode::Primitive(primitive) => self.primitive(primitive, context),
        }
    }

    fn layer_content(&mut self, nodes: &[RenderPaintLayerContentNode], context: Context) {
        nodes.iter().for_each(|node| match node {
            RenderPaintLayerContentNode::Own(run) => self.nodes(&run.nodes, context),
            RenderPaintLayerContentNode::Child(layer) => {
                self.node(&RenderNode::PaintLayer(layer.clone()), context)
            }
            RenderPaintLayerContentNode::ShadowPass { children } => self.layer_content(
                children,
                Context {
                    clip: None,
                    ..context.child(|hasher| 1u8.hash(hasher))
                },
            ),
            RenderPaintLayerContentNode::Clip { clips, children } => {
                self.layer_content(children, context.clipped(clips, 2))
            }
            RenderPaintLayerContentNode::RelaxedClip { clips, children } => self.layer_content(
                children,
                context.child(|hasher| {
                    3u8.hash(hasher);
                    hash_paint_layer_clip_shapes(hasher, clips, FLOAT);
                }),
            ),
            RenderPaintLayerContentNode::Transform {
                transform,
                children,
            } => self.layer_content(
                children,
                Context {
                    transform: context.transform.then(*transform),
                    ..context.child(|hasher| {
                        4u8.hash(hasher);
                        hash_paint_layer_affine2(hasher, *transform, FLOAT);
                    })
                },
            ),
            RenderPaintLayerContentNode::Alpha { alpha, children } => self.layer_content(
                children,
                context.child(|hasher| {
                    5u8.hash(hasher);
                    alpha.to_bits().hash(hasher);
                }),
            ),
        });
    }

    fn primitive(&mut self, primitive: &DrawPrimitive, context: Context) {
        // Video frames change without a scene change.
        if matches!(primitive, DrawPrimitive::Video(..)) {
            self.volatile = true;
        }

        let mut hasher = DefaultHasher::new();
        context.key.hash(&mut hasher);
        hash_paint_layer_draw_primitive(&mut hasher, primitive, FLOAT);

        let device = context
            .transform
            .map_rect_aabb(draw_primitive_visual_bounds(primitive));
        let bounds = match context.clip {
            Some(clip) => match device.intersect(clip) {
                Some(visible) => DamageRect::from_bounds(visible),
                None => Some(DamageRect::EMPTY),
            },
            None => DamageRect::from_bounds(device),
        };

        // Only an integer-aligned plain rectangle has no partially covered pixels.
        let antialiased = context.rounded_clip
            || match primitive {
                DrawPrimitive::Rect(x, y, w, h, _) => !is_pixel_aligned(
                    &context.transform,
                    std::iter::once(Rect {
                        x: *x,
                        y: *y,
                        width: *w,
                        height: *h,
                    }),
                ),
                _ => true,
            };

        self.items.push(DrawItem {
            key: hasher.finish(),
            bounds,
            antialiased,
        });
    }
}

/// True when `rects` map to whole device pixels under `transform`, which must
/// be an integer translation.
fn is_pixel_aligned(transform: &Affine2, mut rects: impl Iterator<Item = Rect>) -> bool {
    let whole = |value: f32| value.fract() == 0.0;
    transform.xx == 1.0
        && transform.yy == 1.0
        && transform.xy == 0.0
        && transform.yx == 0.0
        && whole(transform.tx)
        && whole(transform.ty)
        && rects
            .all(|rect| whole(rect.x) && whole(rect.y) && whole(rect.width) && whole(rect.height))
}
