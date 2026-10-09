# Raster partial redraw

Goal: headless raster frames cost time proportional to what changed, not to
the surface size. On a Tegra 3 (Nexus 7, 800x1280) a one-label change costs
about the same as a full repaint today (~105 ms), because every frame clears
and redraws the whole scene and copies the whole surface twice.

## Design

1. Damage from scene diff (`render_damage.rs`).
   - Flatten a `RenderScene` into draw items: device-space bounds (outset and
     pixel-aligned) plus a key hashing the primitive and its full
     transform/clip/alpha/shadow/paint-layer context.
   - Damage = bounds of items only in the old scene plus items only in the new
     scene (multiset diff). Ordering changes among common items, video
     primitives, font/image snapshot changes, clear colour or size changes
     fall back to full damage.
   - Region = up to N merged rectangles, otherwise their bounding box.
2. Clipped repaint (`SceneRenderer`).
   - Render with the canvas clipped to the damage region; the clear also
     respects the clip, so pixels outside keep the previous frame. Only valid
     for backends with a persistent surface (raster).
3. Copy only what changed (headless raster).
   - Latest-frame store updates damaged rows in place instead of receiving a
     full copy per frame.
   - Full-frame output (default) still sends the whole frame, now in one copy.
   - Opt-in `headless: [partial: true]` sends only the damaged bounding box:
     `{tag, frame, {x, y}}`, where `frame` is a region-sized VideoInterop
     frame. The first frame and any full-damage frame are full size at
     `{0, 0}`. Nothing is sent when nothing changed.

## Validation

- Pixel equality: partial render == full render for scene sequences
  (text change, colour change, move, insert, remove, reorder, scroll,
  transforms, shadows), in Rust tests.
- Nexus 7 benchmark: one-label change, all-rows change, blank; DRM presenter
  consuming partial frames.
