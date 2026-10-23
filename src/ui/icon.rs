//! The tray icon, drawn at the size the system asks for: the app icon's paper plane on its
//! teal tile, in colour while connected and in grey otherwise.

type Color = [f32; 3];
const TILE: Color = [0.055, 0.290, 0.302];
const PLANE_TOP: Color = [0.290, 0.859, 0.761];
const PLANE_BOTTOM: Color = [0.071, 0.620, 0.627];
const FOLD: Color = [0.878, 0.980, 0.960];
/// The plane's outline in tile coordinates: tail, nose, lower tip and the notch between them.
const PLANE: [(f32, f32); 4] = [(0.18, 0.47), (0.80, 0.20), (0.57, 0.82), (0.465, 0.555)];
const SAMPLES: u32 = 4;

/// Straight-alpha `0xAARRGGBB` pixels, top row first.
pub fn pixels(size: u32, running: bool) -> Vec<u32> {
    let mut pixels = Vec::with_capacity((size * size) as usize);
    let step = 1.0 / (size * SAMPLES) as f32;
    // The fold line disappears into the plane below this size.
    let fold = if size >= 24 { 0.6 / size as f32 } else { 0.0 };
    for y in 0..size {
        for x in 0..size {
            let mut sum = [0.0f32; 4];
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (x * SAMPLES + sx) as f32 * step + step / 2.0;
                    let py = (y * SAMPLES + sy) as f32 * step + step / 2.0;
                    if let Some(color) = sample(px, py, fold) {
                        let color = if running { color } else { grey(color) };
                        for channel in 0..3 {
                            sum[channel] += color[channel];
                        }
                        sum[3] += 1.0;
                    }
                }
            }
            let alpha = sum[3] / (SAMPLES * SAMPLES) as f32;
            let channel = |index: usize| {
                if sum[3] > 0.0 {
                    (sum[index] / sum[3] * 255.0).round() as u32
                } else {
                    0
                }
            };
            pixels.push(
                ((alpha * 255.0).round() as u32) << 24
                    | channel(0) << 16
                    | channel(1) << 8
                    | channel(2),
            );
        }
    }
    pixels
}

fn sample(x: f32, y: f32, fold: f32) -> Option<Color> {
    if !in_tile(x, y) {
        return None;
    }
    let [tail, nose, tip, notch] = PLANE;
    if fold > 0.0 && distance_to_segment((x, y), notch, lerp(notch, nose, 0.62)) < fold {
        return Some(FOLD);
    }
    if !in_polygon((x, y), &PLANE) {
        return Some(TILE);
    }
    let t = ((y - nose.1) / (tip.1 - nose.1) + (tail.0 - x) * 0.3).clamp(0.0, 1.0);
    Some(std::array::from_fn(|i| {
        PLANE_TOP[i] + (PLANE_BOTTOM[i] - PLANE_TOP[i]) * t
    }))
}
/// A tile with rounded corners that fills the icon, as tray icons have little room.
fn in_tile(x: f32, y: f32) -> bool {
    const MARGIN: f32 = 0.03;
    const RADIUS: f32 = 0.22;
    let (low, high) = (MARGIN + RADIUS, 1.0 - MARGIN - RADIUS);
    let dx = (low - x).max(x - high).max(0.0);
    let dy = (low - y).max(y - high).max(0.0);
    (MARGIN..=1.0 - MARGIN).contains(&x)
        && (MARGIN..=1.0 - MARGIN).contains(&y)
        && dx * dx + dy * dy <= RADIUS * RADIUS
}
fn in_polygon(point: (f32, f32), polygon: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        if (current.1 > point.1) != (previous.1 > point.1)
            && point.0
                < (previous.0 - current.0) * (point.1 - current.1) / (previous.1 - current.1)
                    + current.0
        {
            inside = !inside;
        }
        previous = current;
    }
    inside
}
fn lerp(a: (f32, f32), b: (f32, f32), t: f32) -> (f32, f32) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}
fn distance_to_segment(point: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (x, y) = lerp(a, b, t);
    ((point.0 - x).powi(2) + (point.1 - y).powi(2)).sqrt()
}
/// A muted grey that stays readable on light and dark panels.
fn grey(color: Color) -> Color {
    let luminance = 0.299 * color[0] + 0.587 * color[1] + 0.114 * color[2];
    [0.30 + luminance * 0.62; 3]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transparent_corners_opaque_plane() {
        for size in [16, 20, 24, 32, 48, 64] {
            for running in [false, true] {
                let pixels = pixels(size, running);
                assert_eq!(pixels.len(), (size * size) as usize);
                assert_eq!(pixels[0] >> 24, 0, "corner of {size}px icon");
                let center = pixels[(size / 2 * size + size / 2) as usize];
                assert_eq!(center >> 24, 255, "centre of {size}px icon");
            }
        }
        // Grey while stopped: every opaque pixel has equal channels.
        assert!(pixels(32, false).iter().all(|p| {
            let (r, g, b) = ((p >> 16) & 255, (p >> 8) & 255, p & 255);
            p >> 24 == 0 || (r == g && g == b)
        }));
    }
    /// Writes RGBA previews for a visual check: FLOWBAR_ICON_PREVIEW=dir cargo test -- --ignored
    #[test]
    #[ignore]
    fn preview() {
        let directory = std::path::PathBuf::from(std::env::var("FLOWBAR_ICON_PREVIEW").unwrap());
        for size in [16, 32, 256] {
            for running in [false, true] {
                let bytes: Vec<u8> = pixels(size, running)
                    .iter()
                    .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8])
                    .collect();
                std::fs::write(directory.join(format!("{size}-{running}.rgba")), bytes).unwrap();
            }
        }
    }
}
