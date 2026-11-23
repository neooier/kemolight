use eframe::egui;
use image::{imageops, ImageFormat};

const LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

pub fn app_icon(size: u32) -> egui::IconData {
    let size = size.max(1);
    let logo = image::load_from_memory_with_format(LOGO_PNG, ImageFormat::Png)
        .expect("embedded KemoLight logo must be a valid PNG")
        .into_rgba8();
    let pixels = imageops::resize(&logo, size, size, imageops::FilterType::Lanczos3);
    egui::IconData {
        rgba: pixels.into_raw(),
        width: size,
        height: size,
    }
}

#[cfg(all(target_os = "linux", feature = "tray"))]
pub fn tray_icon_argb(size: i32) -> ksni::Icon {
    let mut icon = app_icon(size.max(1) as u32);
    // StatusNotifierItem expects bytes in ARGB order.
    for pixel in icon.rgba.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: icon.width as i32,
        height: icon.height as i32,
        data: icon.rgba,
    }
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

    #[cfg(all(target_os = "linux", feature = "tray"))]
    #[test]
    fn tray_icon_uses_argb_byte_order() {
        let window_icon = app_icon(32);
        let tray_icon = tray_icon_argb(32);
        assert_eq!((tray_icon.width, tray_icon.height), (32, 32));
        let expected: Vec<_> = window_icon
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| [a, r, g, b])
            .collect();
        assert_eq!(tray_icon.data, expected);
    }
}
