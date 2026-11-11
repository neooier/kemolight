use eframe::egui;

const LOGO_COLOR: [u8; 4] = [0xF5, 0x9E, 0x0B, 0xFF];

pub fn app_icon(size: u32) -> egui::IconData {
    egui::IconData {
        rgba: rgba_pixels(size),
        width: size,
        height: size,
    }
}

#[cfg(all(target_os = "linux", feature = "tray"))]
pub fn tray_icon_argb(size: i32) -> ksni::Icon {
    let size = size.max(1) as u32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);

    for y in 0..size {
        for x in 0..size {
            let alpha = logo_alpha(x, y, size);
            data.extend_from_slice(&[alpha, LOGO_COLOR[0], LOGO_COLOR[1], LOGO_COLOR[2]]);
        }
    }

    ksni::Icon {
        width: size as i32,
        height: size as i32,
        data,
    }
}

fn rgba_pixels(size: u32) -> Vec<u8> {
    let size = size.max(1);
    let mut data = Vec::with_capacity((size * size * 4) as usize);

    for y in 0..size {
        for x in 0..size {
            let alpha = logo_alpha(x, y, size);
            data.extend_from_slice(&[LOGO_COLOR[0], LOGO_COLOR[1], LOGO_COLOR[2], alpha]);
        }
    }

    data
}

fn logo_alpha(x: u32, y: u32, size: u32) -> u8 {
    let sx = (x as f32 + 0.5) * 64.0 / size as f32;
    let sy = (y as f32 + 0.5) * 64.0 / size as f32;
    let strokes = [
        (32.0, 7.0, 32.0, 21.0, 5.2),
        (32.0, 23.0, 14.0, 9.0, 5.0),
        (32.0, 23.0, 50.0, 9.0, 5.0),
        (8.0, 28.0, 25.0, 28.0, 5.1),
        (39.0, 28.0, 56.0, 28.0, 5.1),
        (25.0, 33.0, 32.0, 39.0, 4.4),
        (39.0, 33.0, 32.0, 39.0, 4.4),
        (14.0, 42.0, 27.0, 42.0, 5.1),
        (37.0, 42.0, 50.0, 42.0, 5.1),
        (32.0, 44.0, 17.0, 58.0, 5.0),
        (32.0, 44.0, 47.0, 58.0, 5.0),
    ];
    let distance = strokes
        .iter()
        .map(|&(ax, ay, bx, by, radius)| distance_to_segment(sx, sy, ax, ay, bx, by) - radius)
        .fold(f32::INFINITY, f32::min);
    let antialias = 64.0 / size as f32;
    ((0.5 - distance / antialias.max(0.001)).clamp(0.0, 1.0) * 255.0).round() as u8
}

fn distance_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let vx = bx - ax;
    let vy = by - ay;
    let wx = px - ax;
    let wy = py - ay;
    let length_squared = vx * vx + vy * vy;
    if length_squared <= f32::EPSILON {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }

    let t = ((wx * vx + wy * vy) / length_squared).clamp(0.0, 1.0);
    let cx = ax + t * vx;
    let cy = ay + t * vy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_has_rgba_pixels() {
        let icon = app_icon(32);
        assert_eq!(icon.width, 32);
        assert_eq!(icon.height, 32);
        assert_eq!(icon.rgba.len(), 32 * 32 * 4);
        assert!(icon
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] > 0));
    }
}
