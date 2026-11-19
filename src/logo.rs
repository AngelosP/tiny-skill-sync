//! The app's logo, drawn from three shapes at any size: a rounded tile, two sync arrows and a spark (a skill).
//!
//! Pure: [`draw`] gives the pixels, which the tray and the window turn into icons at the exact size their DPI asks for
//! (`win::ui::tray`), and [`svg`] the same drawing as SVG. The files in `assets\` (the exes' icon, the logo for the
//! README and the site, the favicon) are written from these two by a test, which fails when they differ.

pub const BLUE: u32 = 0x0F6CBD;
pub const GREY: u32 = 0x8A8A8A;
pub const AMBER: u32 = 0xD98C00;
pub const RED: u32 = 0xD13438;
pub const WHITE: u32 = 0xFFFFFF;

/// The tile: half its width and the radius of its corners, and the width of its rim when hollow.
const TILE: f32 = 0.94;
const CORNER: f32 = 0.38;
const RIM: f32 = 0.2;
/// The two sync arrows: a circle, the half width of the stroke, and the direction from the centre of each arrow's tail
/// and head, as `(cos, sin)` of 200 to 320 degrees and of 20 to 140 degrees (y down, so 270 is the top). Both turn
/// clockwise on screen. The directions are written out so that drawing needs no trigonometry, which would add to the
/// exe; a test checks them against the angles.
const ARROW_RADIUS: f32 = 0.62;
const STROKE: f32 = 0.085;
const ARROWS: [[(f32, f32); 2]; 2] = [[(-0.939_692_6, -0.342_020_15), (0.766_044_4, -0.642_787_6)], [(0.939_692_6, 0.342_020_15), (-0.766_044_4, 0.642_787_6)]];
const HEAD_WIDTH: f32 = 2.3 * STROKE;
const HEAD_LENGTH: f32 = 2.6 * STROKE;
/// The spark: four concave points, each a parabola (a quadratic curve through the centre), reaching this far.
const SPARK: f32 = 0.36;
/// Each pixel is the average of this many samples squared.
const SAMPLES: u32 = 4;

/// How the logo is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// A tile of this colour with a white glyph.
    Solid(u32),
    /// The rim of the tile and the glyph in this colour, see-through between them.
    Hollow(u32),
}

/// Straight-alpha `0xAARRGGBB` pixels of a `size` by `size` logo, row by row from the top.
pub fn draw(look: Look, size: u32) -> Vec<u32> {
    let n = size * SAMPLES;
    let unit = |i: u32| (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
    let mut out = Vec::with_capacity((size * size) as usize);
    for row in 0..size {
        for col in 0..size {
            let (mut covered, mut sum) = (0u32, [0u32; 3]);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    if let Some(colour) = colour_at(look, unit(col * SAMPLES + sx), unit(row * SAMPLES + sy)) {
                        covered += 1;
                        for (k, s) in sum.iter_mut().enumerate() {
                            *s += (colour >> (16 - 8 * k)) & 0xFF;
                        }
                    }
                }
            }
            out.push(if covered == 0 {
                0
            } else {
                let alpha = (covered * 255 + SAMPLES * SAMPLES / 2) / (SAMPLES * SAMPLES);
                let channel = |s: u32| (s + covered / 2) / covered;
                (alpha << 24) | (channel(sum[0]) << 16) | (channel(sum[1]) << 8) | channel(sum[2])
            });
        }
    }
    out
}

/// The colour at a point in unit coordinates (-1 to 1, y down), or `None` where the logo is see-through.
fn colour_at(look: Look, x: f32, y: f32) -> Option<u32> {
    let glyph = in_spark(x, y) || ARROWS.iter().any(|&[tail, head]| in_arrow(x, y, tail, head));
    match look {
        Look::Solid(colour) => in_tile(x, y, TILE, CORNER).then_some(if glyph { WHITE } else { colour }),
        Look::Hollow(colour) => (glyph || (in_tile(x, y, TILE, CORNER) && !in_tile(x, y, TILE - RIM, CORNER - RIM))).then_some(colour),
    }
}

fn length(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// `b` is clockwise of `a` (y down), by less than half a turn.
fn cross(a: (f32, f32), b: (f32, f32)) -> f32 {
    a.0 * b.1 - a.1 * b.0
}

fn in_tile(x: f32, y: f32, half: f32, corner: f32) -> bool {
    let (qx, qy) = (x.abs() - (half - corner), y.abs() - (half - corner));
    length(qx.max(0.0), qy.max(0.0)) + qx.max(qy).min(0.0) <= corner
}

fn in_spark(x: f32, y: f32) -> bool {
    (x.abs() / SPARK).sqrt() + (y.abs() / SPARK).sqrt() <= 1.0
}

/// On the arrow that turns clockwise from `tail` to `head` (directions, less than half a turn apart): the stroke with a
/// round tail, or the head.
fn in_arrow(x: f32, y: f32, tail: (f32, f32), head: (f32, f32)) -> bool {
    let between = cross(tail, (x, y)) >= 0.0 && cross((x, y), head) >= 0.0;
    if between && (length(x, y) - ARROW_RADIUS).abs() <= STROKE {
        return true;
    }
    if length(x - ARROW_RADIUS * tail.0, y - ARROW_RADIUS * tail.1) <= STROKE {
        return true;
    }
    let [a, b, c] = arrowhead(head);
    in_triangle((x, y), a, b, c)
}

/// The corners of the arrowhead at `head`: two on the circle's radius, and the tip, a little further clockwise.
fn arrowhead((nx, ny): (f32, f32)) -> [(f32, f32); 3] {
    let (px, py) = (ARROW_RADIUS * nx, ARROW_RADIUS * ny);
    let (tx, ty) = (-ny, nx);
    [(px + nx * HEAD_WIDTH, py + ny * HEAD_WIDTH), (px - nx * HEAD_WIDTH, py - ny * HEAD_WIDTH), (px + tx * HEAD_LENGTH, py + ty * HEAD_LENGTH)]
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let side = |u: (f32, f32), v: (f32, f32)| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (s1, s2, s3) = (side(a, b), side(b, c), side(c, a));
    (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0)
}

/// The logo as an SVG file, 256 units square.
pub fn svg() -> String {
    // Unit coordinates to SVG units, rounded to a hundredth.
    let u = |v: f32| {
        let s = format!("{:.2}", (v + 1.0) * 128.0);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let len = |v: f32| u(v - 1.0);
    let mut body = format!("<rect x=\"{0}\" y=\"{0}\" width=\"{1}\" height=\"{1}\" rx=\"{2}\" fill=\"#{BLUE:06X}\"/>", u(-TILE), len(2.0 * TILE), len(CORNER));
    for [tail, head] in ARROWS {
        let at = |(x, y): (f32, f32)| (ARROW_RADIUS * x, ARROW_RADIUS * y);
        let (s, e) = (at(tail), at(head));
        // Less than half a turn, clockwise.
        body += &format!(
            "<path d=\"M{} {} A{r} {r} 0 0 1 {} {}\" fill=\"none\" stroke=\"#fff\" stroke-width=\"{}\" stroke-linecap=\"round\"/>",
            u(s.0),
            u(s.1),
            u(e.0),
            u(e.1),
            len(2.0 * STROKE),
            r = len(ARROW_RADIUS)
        );
        let corners: Vec<String> = arrowhead(head).iter().map(|&(x, y)| format!("{},{}", u(x), u(y))).collect();
        body += &format!("<polygon points=\"{}\" fill=\"#fff\"/>", corners.join(" "));
    }
    let (near, far) = (u(-SPARK), u(SPARK));
    body += &format!("<path d=\"M128 {near} Q128 128 {far} 128 Q128 128 128 {far} Q128 128 {near} 128 Q128 128 128 {near}Z\" fill=\"#fff\"/>");
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\">{body}</svg>\n")
}

#[cfg(test)]
mod files;

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    /// The sizes in the exes' icon: the ones Windows asks for at 100% to 200% (small icons, the taskbar, Explorer's
    /// views), and 256 for the largest views.
    const EXE_ICON: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

    /// Every file in `assets\` and what it should hold.
    fn assets() -> Vec<(String, Vec<u8>)> {
        let png = |size: u32| files::png(&draw(Look::Solid(BLUE), size), size);
        let mut all = vec![("logo.svg".to_string(), svg().into_bytes())];
        for size in [16, 32, 48, 64, 128, 256] {
            all.push((format!("logo-{size}.png"), png(size)));
        }
        all.push(("favicon.ico".to_string(), files::ico(&[16, 32, 48].map(|s| (s, png(s))))));
        all.push(("tiny-skill-sync.ico".to_string(), files::ico(&EXE_ICON.map(|s| (s, png(s))))));
        all
    }

    #[test]
    fn the_files_in_assets_are_what_the_code_draws() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        let write = std::env::var("TSS_WRITE_ASSETS").as_deref() == Ok("1");
        for (name, want) in assets() {
            let path = dir.join(&name);
            if write {
                std::fs::create_dir_all(&dir).expect("assets can be made");
                std::fs::write(&path, &want).expect("an asset can be written");
                continue;
            }
            let mut got = std::fs::read(&path).unwrap_or_default();
            // Git may check text out with CRLF.
            if name.ends_with(".svg") {
                got.retain(|&b| b != b'\r');
            }
            assert!(got == want, "assets\\{name} is not what logo.rs draws: run scripts\\dev.ps1 logo, then look at it");
        }
    }

    /// The pixel at `(u, v)` in unit coordinates (-1 to 1, y down).
    fn at(pixels: &[u32], size: u32, u: f32, v: f32) -> u32 {
        let px = |c: f32| (((c + 1.0) / 2.0 * size as f32) as u32).min(size - 1);
        pixels[(px(v) * size + px(u)) as usize]
    }

    #[test]
    fn a_solid_logo_is_a_coloured_tile_with_a_white_spark_and_white_arrows() {
        for size in ALL {
            let p = draw(Look::Solid(BLUE), size);
            assert_eq!(p.len(), (size * size) as usize);
            for (what, u, v) in [("the spark", 0.0, 0.0), ("the top arrow", 0.0, -ARROW_RADIUS), ("the bottom arrow", 0.0, ARROW_RADIUS)] {
                let px = at(&p, size, u, v);
                assert!(px >> 24 == 0xFF && (px & 0xFF) > 0xC0, "{what} is mostly white at {size} px: {px:08x}");
            }
            assert_eq!(at(&p, size, 0.28, 0.28), 0xFF00_0000 | BLUE, "the tile between the glyphs at {size} px");
            assert_eq!(at(&p, size, 0.85, 0.0), 0xFF00_0000 | BLUE, "the edge of the tile at {size} px");
            assert_eq!(p[0], 0, "the rounded corner is see-through at {size} px");
        }
    }

    #[test]
    fn a_hollow_logo_keeps_its_rim_and_glyph_and_lets_the_taskbar_through() {
        for size in ALL {
            let p = draw(Look::Hollow(GREY), size);
            for (what, u, v) in [("the spark", 0.0, 0.0), ("the top arrow", 0.0, -ARROW_RADIUS)] {
                let px = at(&p, size, u, v);
                assert!(px >> 24 > 0x80 && px & 0xFF_FFFF == GREY, "{what} is mostly there at {size} px: {px:08x}");
            }
            assert_eq!(at(&p, size, 0.85, 0.0), 0xFF00_0000 | GREY, "the rim at {size} px");
            assert_eq!(at(&p, size, 0.28, 0.28) >> 24, 0, "inside the rim, between the glyphs, at {size} px");
            assert_eq!(p[0], 0, "the rounded corner at {size} px");
        }
    }

    #[test]
    fn edges_are_smoothed_and_never_carry_a_colour_the_logo_does_not_use() {
        let p = draw(Look::Solid(RED), 16);
        assert!(p.iter().any(|&px| (1..255).contains(&(px >> 24))), "a corner pixel is partly covered");
        // Where the tile is only partly covered, the colour is the tile's (straight alpha), not a mix with black.
        for px in p.iter().filter(|&&px| (1..255).contains(&(px >> 24))) {
            let rgb = px & 0xFF_FFFF;
            let channels = |c: u32| [(c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF];
            for (got, (lo, hi)) in channels(rgb).iter().zip(channels(RED).iter().zip(channels(WHITE).iter())) {
                assert!((*lo.min(hi)..=*lo.max(hi)).contains(got), "{px:08x} lies between red and white");
            }
        }
        assert_eq!(draw(Look::Solid(RED), 16), p, "the same pixels every time");
    }

    #[test]
    fn the_arrows_run_where_their_angles_say() {
        for ([tail, head], (from, to)) in ARROWS.iter().zip([(200.0f32, 320.0f32), (20.0, 140.0)]) {
            for (got, deg) in [(tail, from), (head, to)] {
                let r = deg.to_radians();
                assert!((got.0 - r.cos()).abs() < 1e-6 && (got.1 - r.sin()).abs() < 1e-6, "{deg} degrees: {got:?}");
            }
        }
    }

    #[test]
    fn the_svg_draws_the_same_shapes() {
        let svg = svg();
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\">"));
        assert!(svg.contains(&format!("fill=\"#{BLUE:06X}\"")), "{svg}");
        assert_eq!(svg.matches("<polygon").count(), 2, "two arrowheads: {svg}");
        assert_eq!(svg.matches(" A").count(), 2, "two arcs: {svg}");
        assert_eq!(svg.matches(" Q").count(), 4, "a spark of four curves: {svg}");
    }
}
