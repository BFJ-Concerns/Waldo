use std::io::Cursor;

use image::{ImageEncoder, codecs::png::PngEncoder};
use wayland_client::protocol::wl_shm;

use crate::{CaptureError, shm::ShmBuffer};

pub(crate) fn png(buffer: &ShmBuffer, y_inverted: bool) -> Result<Vec<u8>, CaptureError> {
    png_bytes(
        &buffer.map,
        buffer.width,
        buffer.height,
        buffer.stride,
        buffer.format,
        y_inverted,
    )
}

fn png_bytes(
    source: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_inverted: bool,
) -> Result<Vec<u8>, CaptureError> {
    let rgba = rgba(source, width, height, stride, format, y_inverted)?;

    let mut encoded = Cursor::new(Vec::new());
    PngEncoder::new(&mut encoded).write_image(
        &rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(encoded.into_inner())
}

fn rgba(
    source: &[u8],
    buffer_width: u32,
    buffer_height: u32,
    buffer_stride: u32,
    format: wl_shm::Format,
    y_inverted: bool,
) -> Result<Vec<u8>, CaptureError> {
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        return Err(CaptureError::UnsupportedFormat(format!("{:?}", format)));
    }

    let pixel_count = usize::try_from(buffer_width)
        .ok()
        .and_then(|width| {
            usize::try_from(buffer_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| CaptureError::InvalidBuffer("RGBA size overflow".into()))?;
    let capacity = pixel_count
        .checked_mul(4)
        .ok_or_else(|| CaptureError::InvalidBuffer("RGBA byte size overflow".into()))?;
    let mut rgba = Vec::with_capacity(capacity);
    let stride = usize::try_from(buffer_stride)
        .map_err(|_| CaptureError::InvalidBuffer("stride exceeds usize".into()))?;
    let width = usize::try_from(buffer_width)
        .map_err(|_| CaptureError::InvalidBuffer("width exceeds usize".into()))?;
    let height = usize::try_from(buffer_height)
        .map_err(|_| CaptureError::InvalidBuffer("height exceeds usize".into()))?;

    for destination_y in 0..height {
        let source_y = if y_inverted {
            height - destination_y - 1
        } else {
            destination_y
        };
        let row_start = source_y * stride;
        for x in 0..width {
            let offset = row_start + x * 4;
            let bytes: [u8; 4] = source
                .get(offset..offset + 4)
                .ok_or_else(|| CaptureError::InvalidBuffer("pixel exceeds mapped memory".into()))?
                .try_into()
                .expect("the requested source slice is exactly four bytes");
            let pixel = u32::from_ne_bytes(bytes);
            rgba.push(((pixel >> 16) & 0xff) as u8);
            rgba.push(((pixel >> 8) & 0xff) as u8);
            rgba.push((pixel & 0xff) as u8);
            rgba.push(if format == wl_shm::Format::Argb8888 {
                ((pixel >> 24) & 0xff) as u8
            } else {
                0xff
            });
        }
    }

    Ok(rgba)
}

#[cfg(test)]
mod tests {
    use image::ImageFormat;
    use wayland_client::protocol::wl_shm;

    use super::{png_bytes, rgba};

    fn native_pixel(argb: u32) -> [u8; 4] {
        argb.to_ne_bytes()
    }

    #[test]
    fn converts_native_xrgb_and_forces_an_opaque_alpha_channel() {
        let bytes = native_pixel(0x0012_3456);

        let converted = rgba(&bytes, 1, 1, 4, wl_shm::Format::Xrgb8888, false).unwrap();

        assert_eq!(converted, [0x12, 0x34, 0x56, 0xff]);
    }

    #[test]
    fn preserves_argb_alpha_and_corrects_y_inversion() {
        let mut bytes = Vec::new();
        bytes.extend(native_pixel(0x8011_2233));
        bytes.extend(native_pixel(0xff44_5566));

        let converted = rgba(&bytes, 1, 2, 4, wl_shm::Format::Argb8888, true).unwrap();

        assert_eq!(converted, [0x44, 0x55, 0x66, 0xff, 0x11, 0x22, 0x33, 0x80]);
    }

    #[test]
    fn rejects_a_stride_that_points_past_the_mapping() {
        let error = rgba(&[0; 4], 1, 2, 8, wl_shm::Format::Xrgb8888, false).unwrap_err();

        assert!(error.to_string().contains("pixel exceeds mapped memory"));
    }

    #[test]
    fn small_damage_still_encodes_the_complete_seeded_frame() {
        let width = 4;
        let height = 3;
        let damage = (1_u32, 1_u32, 1_u32, 1_u32);
        let mut source = Vec::new();
        let mut expected = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let red = (x * 37 + y * 11) as u8;
                let green = (x * 13 + y * 53) as u8;
                let blue = (x * 71 + y * 7) as u8;
                source.extend(native_pixel(
                    u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue),
                ));
                expected.extend([red, green, blue, 0xff]);
            }
        }
        assert!(damage.2 * damage.3 < width * height);

        // Screencopy damage describes which pixels changed; the compositor still
        // copied a complete buffer, so encoding must consume the complete extent.
        let png = png_bytes(
            &source,
            width,
            height,
            width * 4,
            wl_shm::Format::Xrgb8888,
            false,
        )
        .unwrap();
        let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .unwrap()
            .into_rgba8();

        assert_eq!(decoded.dimensions(), (width, height));
        assert_eq!(decoded.into_raw(), expected);
    }
}
