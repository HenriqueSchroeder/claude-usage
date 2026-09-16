use crate::render::Rgb;

const SS: u32 = 4; // supersampling factor, for anti-aliasing without a rasterizer
const TRACK_ALPHA: u8 = 60;

/// Renders a progress ring as ARGB32, `size * size * 4` bytes.
///
/// Anti-aliasing is done by supersampling each pixel SS*SS times and
/// averaging coverage. That is cheaper in dependencies than pulling in a
/// rasterizer, and at 22px the quality is indistinguishable.
pub fn ring(percent: f32, color: Rgb, size: u32) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let pct = percent.clamp(0.0, 100.0) / 100.0;
    let mut out = vec![0u8; (size * size * 4) as usize];

    let center = size as f32 / 2.0;
    let outer = center - 1.0;
    let inner = outer * 0.62;
    let sweep = pct * std::f32::consts::TAU;

    for y in 0..size {
        for x in 0..size {
            let mut on_arc = 0u32;
            let mut on_track = 0u32;

            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let dx = px - center;
                    let dy = py - center;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist < inner || dist > outer {
                        continue;
                    }
                    on_track += 1;
                    // Angle measured clockwise from 12 o'clock.
                    let mut a = dx.atan2(-dy);
                    if a < 0.0 {
                        a += std::f32::consts::TAU;
                    }
                    if a <= sweep {
                        on_arc += 1;
                    }
                }
            }

            if on_track == 0 {
                continue;
            }
            let total = (SS * SS) as f32;
            let arc_cov = on_arc as f32 / total;
            let track_cov = (on_track - on_arc) as f32 / total;
            let alpha = (arc_cov * 255.0 + track_cov * TRACK_ALPHA as f32).min(255.0) as u8;

            let i = ((y * size + x) * 4) as usize;
            out[i] = alpha;
            out[i + 1] = color.0;
            out[i + 2] = color.1;
            out[i + 3] = color.2;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Rgb;

    const RED: Rgb = Rgb(0xe0, 0x1b, 0x24);

    #[test]
    fn output_has_the_exact_argb_length() {
        assert_eq!(ring(50.0, RED, 22).len(), 22 * 22 * 4);
        assert_eq!(ring(50.0, RED, 44).len(), 44 * 44 * 4);
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(ring(73.0, RED, 22), ring(73.0, RED, 22));
    }

    #[test]
    fn different_percentages_differ() {
        assert_ne!(ring(10.0, RED, 22), ring(90.0, RED, 22));
    }

    #[test]
    fn full_ring_has_more_total_alpha_than_empty() {
        let total_alpha = |b: &[u8]| b.chunks(4).map(|p| p[0] as u32).sum::<u32>();
        assert!(total_alpha(&ring(100.0, RED, 22)) > total_alpha(&ring(0.0, RED, 22)));
    }

    #[test]
    fn percentages_are_clamped_not_panicking() {
        assert_eq!(ring(-50.0, RED, 22), ring(0.0, RED, 22));
        assert_eq!(ring(500.0, RED, 22), ring(100.0, RED, 22));
    }

    #[test]
    fn zero_size_yields_empty_not_panic() {
        assert!(ring(50.0, RED, 0).is_empty());
    }
}
