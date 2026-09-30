//! A small software rasterizer for the overlay: premultiplied ARGB pixels, antialiased rings,
//! lines and rounded rectangles, an RGBA image blit, and text through fontdue. Everything the
//! overlay draws is a few hundred pixels across, so doing it on the CPU is simpler than a GPU path
//! and fast enough to redraw every frame of an animation.

/// Straight (not premultiplied) RGBA.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub fn with_alpha(self, a: f32) -> Rgba {
        Rgba(
            self.0,
            self.1,
            self.2,
            (f32::from(self.3) * a.clamp(0.0, 1.0)).round() as u8,
        )
    }

    /// Premultiplied ARGB in a u32, what `wl_shm` ARGB8888 holds on a little-endian machine.
    pub fn premultiplied(self) -> u32 {
        let Rgba(r, g, b, a) = self;
        let m = |c: u8| (u32::from(c) * u32::from(a) / 255) & 0xff;
        (u32::from(a) << 24) | (m(r) << 16) | (m(g) << 8) | m(b)
    }
}

pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w * h) as usize],
        }
    }

    pub fn filled(w: u32, h: u32, c: Rgba) -> Self {
        Self {
            w,
            h,
            px: vec![c.premultiplied(); (w * h) as usize],
        }
    }

    /// Composite `c` with coverage `cov` (0..1) over the pixel at (x, y).
    fn blend(&mut self, x: i64, y: i64, c: Rgba, cov: f32) {
        if x < 0 || y < 0 || x >= i64::from(self.w) || y >= i64::from(self.h) || cov <= 0.0 {
            return;
        }
        let src = c.with_alpha(cov).premultiplied();
        let i = (y as u32 * self.w + x as u32) as usize;
        self.px[i] = over(src, self.px[i]);
    }

    /// Every pixel whose centre is within `reach` of the shape, with coverage from `dist`: the
    /// signed distance from the pixel centre to the shape's edge (negative inside).
    fn shade(&mut self, bounds: (f32, f32, f32, f32), c: Rgba, dist: impl Fn(f32, f32) -> f32) {
        let (x0, y0, x1, y1) = bounds;
        for y in (y0.floor() as i64 - 1)..=(y1.ceil() as i64 + 1) {
            for x in (x0.floor() as i64 - 1)..=(x1.ceil() as i64 + 1) {
                let d = dist(x as f32 + 0.5, y as f32 + 0.5);
                self.blend(x, y, c, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// A circle outline centred on (cx, cy), radius `r`, stroke `width`.
    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Rgba) {
        let reach = r + width;
        self.shade(
            (cx - reach, cy - reach, cx + reach, cy + reach),
            c,
            |x, y| ((x - cx).hypot(y - cy) - r).abs() - width / 2.0,
        );
    }

    /// A filled disc.
    pub fn disc(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.shade((cx - r, cy - r, cx + r, cy + r), c, |x, y| {
            (x - cx).hypot(y - cy) - r
        });
    }

    /// A line segment with round caps.
    pub fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: Rgba) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        let half = width / 2.0;
        let bounds = (
            a.0.min(b.0) - half,
            a.1.min(b.1) - half,
            a.0.max(b.0) + half,
            a.1.max(b.1) + half,
        );
        self.shade(bounds, c, |x, y| {
            let t = (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0);
            (x - (a.0 + t * dx)).hypot(y - (a.1 + t * dy)) - half
        });
    }

    /// A filled rectangle with rounded corners.
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, c: Rgba) {
        let (cx, cy, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - radius, h / 2.0 - radius);
        self.shade((x, y, x + w, y + h), c, |px, py| {
            let (qx, qy) = ((px - cx).abs() - hw, (py - cy).abs() - hh);
            qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
        });
    }

    /// Straight RGBA rows (`iw` by `ih`) drawn with their top-left at (x, y), scaled to `side` px
    /// square by averaging the source pixels each target pixel covers (premultiplied, so edges
    /// against transparency stay clean).
    pub fn image(&mut self, rgba: &[u8], iw: u32, ih: u32, x: i64, y: i64, side: u32) {
        for dy in 0..side {
            for dx in 0..side {
                let (sx0, sx1) = (dx * iw / side, ((dx + 1) * iw).div_ceil(side).max(dx * iw / side + 1));
                let (sy0, sy1) = (dy * ih / side, ((dy + 1) * ih).div_ceil(side).max(dy * ih / side + 1));
                let (mut acc, mut n) = ([0f32; 4], 0f32);
                for sy in sy0..sy1.min(ih) {
                    for sx in sx0..sx1.min(iw) {
                        let i = ((sy * iw + sx) * 4) as usize;
                        let a = f32::from(rgba[i + 3]) / 255.0;
                        acc[0] += f32::from(rgba[i]) * a;
                        acc[1] += f32::from(rgba[i + 1]) * a;
                        acc[2] += f32::from(rgba[i + 2]) * a;
                        acc[3] += a;
                        n += 1.0;
                    }
                }
                if n == 0.0 || acc[3] <= 0.0 {
                    continue;
                }
                let alpha = acc[3] / n;
                let c = Rgba((acc[0] / acc[3]) as u8, (acc[1] / acc[3]) as u8, (acc[2] / acc[3]) as u8, 255);
                self.blend(x + i64::from(dx), y + i64::from(dy), c, alpha);
            }
        }
    }

    /// A filled polygon (even-odd), antialiased by 4x4 supersampling.
    pub fn polygon(&mut self, pts: &[(f32, f32)], c: Rgba) {
        let (x0, x1) = pts.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (y0, y1) = pts.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        let inside = |px: f32, py: f32| {
            let mut odd = false;
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if (a.1 > py) != (b.1 > py) && px < a.0 + (py - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                    odd = !odd;
                }
            }
            odd
        };
        for y in y0.floor() as i64..=y1.ceil() as i64 {
            for x in x0.floor() as i64..=x1.ceil() as i64 {
                let mut hits = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        if inside(x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0) {
                            hits += 1;
                        }
                    }
                }
                self.blend(x, y, c, hits as f32 / 16.0);
            }
        }
    }

    /// Text on one line with its baseline at `baseline`, starting at `x`; returns the end x.
    pub fn text(
        &mut self,
        font: &fontdue::Font,
        px: f32,
        x: f32,
        baseline: f32,
        s: &str,
        c: Rgba,
    ) -> f32 {
        let mut pen = x;
        for ch in s.chars() {
            let (m, cov) = font.rasterize(ch, px);
            let (gx, gy) = (
                pen.round() as i64 + i64::from(m.xmin),
                (baseline - m.height as f32 - m.ymin as f32).round() as i64,
            );
            for row in 0..m.height {
                for col in 0..m.width {
                    let a = f32::from(cov[row * m.width + col]) / 255.0;
                    self.blend(gx + col as i64, gy + row as i64, c, a);
                }
            }
            pen += m.advance_width;
        }
        pen
    }
}

/// Width of `s` at `px`.
pub fn text_width(font: &fontdue::Font, px: f32, s: &str) -> f32 {
    s.chars().map(|ch| font.metrics(ch, px).advance_width).sum()
}

/// `s`, cut with an ellipsis to fit `max` px.
pub fn fit(font: &fontdue::Font, px: f32, s: &str, max: f32) -> String {
    if text_width(font, px, s) <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let budget = max - text_width(font, px, "…");
    for ch in s.chars() {
        if text_width(font, px, &out) + font.metrics(ch, px).advance_width > budget {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// Porter-Duff "over" on premultiplied ARGB.
fn over(src: u32, dst: u32) -> u32 {
    let inv = 255 - (src >> 24);
    let ch = |shift: u32| {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        (s + (d * inv + 127) / 255).min(255) << shift
    };
    ch(24) | ch(16) | ch(8) | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_premultiplied_argb() {
        assert_eq!(Rgba(0xff, 0x80, 0x00, 0xff).premultiplied(), 0xffff8000);
        assert_eq!(Rgba(0xff, 0xff, 0xff, 0x80).premultiplied(), 0x80808080);
        assert_eq!(Rgba(0, 0, 0, 0).premultiplied(), 0);
    }

    #[test]
    fn over_keeps_opaque_sources_and_leaves_clear_ones() {
        assert_eq!(over(0xff112233, 0xffabcdef), 0xff112233);
        assert_eq!(over(0, 0xffabcdef), 0xffabcdef);
        assert_eq!(over(0x80000000, 0xffffffff) >> 24, 0xff);
    }

    #[test]
    fn a_ring_is_drawn_on_its_radius_and_not_at_its_centre() {
        let mut c = Canvas::new(40, 40);
        c.ring(20.0, 20.0, 12.0, 3.0, Rgba(255, 0, 0, 255));
        let a = |x: u32, y: u32| c.px[(y * 40 + x) as usize] >> 24;
        assert_eq!(a(20, 20), 0, "hollow");
        assert_eq!(a(32, 20), 0xff, "on the ring");
        assert_eq!(a(39, 20), 0, "outside");
    }

    #[test]
    fn a_line_covers_its_span_only() {
        let mut c = Canvas::new(30, 10);
        c.line((2.0, 5.0), (27.0, 5.0), 2.0, Rgba(0, 255, 0, 255));
        let a = |x: u32, y: u32| c.px[(y * 30 + x) as usize] >> 24;
        assert!(a(15, 4) > 0x80 && a(15, 5) > 0x80);
        assert_eq!(a(15, 0), 0);
    }

    #[test]
    fn a_polygon_fills_its_inside_only() {
        let mut c = Canvas::new(10, 10);
        c.polygon(&[(1.0, 1.0), (9.0, 1.0), (1.0, 9.0)], Rgba(0, 0, 255, 255));
        let a = |x: u32, y: u32| c.px[(y * 10 + x) as usize] >> 24;
        assert_eq!(a(2, 2), 0xff);
        assert_eq!(a(8, 8), 0);
    }

    #[test]
    fn a_downscaled_image_averages_instead_of_dropping_pixels() {
        // 2x2 source: one opaque red pixel, three clear ones -> one pixel of quarter coverage.
        let src = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut c = Canvas::new(1, 1);
        c.image(&src, 2, 2, 0, 0, 1);
        let px = c.px[0];
        assert!((px >> 24) > 0x30 && (px >> 24) < 0x50, "{px:08x}");
    }

    #[test]
    fn the_robot_asset_is_a_32px_rgba_image() {
        let (header, pixels) =
            qoi::decode_to_vec(include_bytes!("../assets/robot-32.qoi")).unwrap();
        assert_eq!(
            (header.width, header.height, pixels.len()),
            (32, 32, 32 * 32 * 4)
        );
        assert!(
            pixels.chunks_exact(4).any(|p| p[3] == 255)
                && pixels.chunks_exact(4).any(|p| p[3] == 0)
        );
    }
}
