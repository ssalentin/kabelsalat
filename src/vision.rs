//! Images for the agent: what the user attaches and what the app
//! screenshots, sized and encoded for a model that can see.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};

/// The longest side a model gets, in pixels. Rack labels stay readable at
/// this size and an app screenshot costs about 1.5k input tokens.
pub const MAX_SIDE: usize = 1400;
/// A JPEG is passed on as the file it is (there is no JPEG decoder here).
const MAX_JPEG_BYTES: usize = 3_000_000;
/// The largest file `from_file` reads.
pub const MAX_FILE_BYTES: u64 = 40_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

/// One encoded image on its way to a model.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub name: String,
    pub format: Format,
    /// The encoded file. Shared: the UI, the worker and each request hold it.
    pub bytes: Arc<[u8]>,
    /// Width and height in pixels; None for a JPEG, which is not decoded.
    pub size: Option<[usize; 2]>,
}

impl Image {
    /// An image file's bytes. A PNG is decoded and scaled down to
    /// `MAX_SIDE`; a JPEG is taken as it is.
    pub fn from_file(name: &str, bytes: &[u8]) -> Result<Image, String> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            let (size, rgba) = decode_png(bytes).map_err(|e| format!("{name} is not a readable PNG: {e}"))?;
            return Image::from_rgba(name, size, &rgba);
        }
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            if bytes.len() > MAX_JPEG_BYTES {
                return Err(format!("{name} is {} MB: a JPEG can be at most {} MB (a PNG of any size is scaled down)",
                    bytes.len() / 1_000_000, MAX_JPEG_BYTES / 1_000_000));
            }
            return Ok(Image { name: name.to_string(), format: Format::Jpeg, bytes: bytes.into(), size: None });
        }
        Err(format!("{name} is neither a PNG nor a JPEG"))
    }

    /// Raw RGBA pixels (a screenshot, a pasted image), scaled down to
    /// `MAX_SIDE` and encoded as PNG.
    pub fn from_rgba(name: &str, size: [usize; 2], rgba: &[u8]) -> Result<Image, String> {
        let [w, h] = size;
        if w == 0 || h == 0 || rgba.len() != w * h * 4 {
            return Err(format!("{name}: {w}x{h} pixels do not fit {} bytes", rgba.len()));
        }
        let (size, rgba) = downscale(size, rgba, MAX_SIDE);
        let bytes = encode_png(size, &rgba).map_err(|e| format!("{name}: PNG encoding failed: {e}"))?;
        Ok(Image { name: name.to_string(), format: Format::Png, bytes: bytes.into(), size: Some(size) })
    }

    /// One line that stands for the image in chips and in the conversation.
    pub fn describe(&self) -> String {
        match self.size {
            Some([w, h]) => format!("{} ({w}×{h} px)", self.name),
            None => format!("{} ({} KB)", self.name, self.bytes.len().div_ceil(1000)),
        }
    }

    /// Decoded RGBA thumbnail pixels scaled to at most `max_side` on the longer edge.
    pub fn thumbnail_rgba(&self, max_side: usize) -> Option<([usize; 2], Vec<u8>)> {
        match self.format {
            Format::Png => decode_png(&self.bytes).ok().map(|(sz, rgba)| downscale(sz, &rgba, max_side)),
            Format::Jpeg => None,
        }
    }

    pub fn base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }

    /// The image as a content part of a chat message.
    pub fn chat_part(&self) -> Value {
        let mime = match self.format { Format::Png => "image/png", Format::Jpeg => "image/jpeg" };
        json!({ "type": "image_url", "image_url": { "url": format!("data:{mime};base64,{}", self.base64()) } })
    }

    /// The image as an entry of a decisions request's `assets`.
    pub fn decision_asset(&self) -> Value {
        let format = match self.format { Format::Png => "png", Format::Jpeg => "jpeg" };
        json!({ "type": "image", "format": format, "data_base64": self.base64() })
    }
}

/// A PNG file's pixels as RGBA, with their width and height.
pub fn decode_png(bytes: &[u8]) -> Result<([usize; 2], Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("the image is too large")?];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let pixels = &buf[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => return Err("indexed colour was not expanded".into()),
    };
    Ok(([frame.width as usize, frame.height as usize], rgba))
}

/// Opaque images are written as RGB: an app screenshot is a quarter smaller.
fn encode_png(size: [usize; 2], rgba: &[u8]) -> Result<Vec<u8>, String> {
    let opaque = rgba.chunks_exact(4).all(|p| p[3] == 255);
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size[0] as u32, size[1] as u32);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_color(if opaque { png::ColorType::Rgb } else { png::ColorType::Rgba });
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    if opaque {
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
    } else {
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Scale RGBA pixels down so the longer side is at most `max_side`: each
/// output pixel is the mean of the input pixels it covers.
fn downscale(size: [usize; 2], rgba: &[u8], max_side: usize) -> ([usize; 2], Vec<u8>) {
    let [w, h] = size;
    let longer = w.max(h);
    if longer <= max_side {
        return (size, rgba.to_vec());
    }
    let (nw, nh) = ((w * max_side / longer).max(1), (h * max_side / longer).max(1));
    let mut out = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1));
            let mut sum = [0u32; 4];
            for row in y0..y1 {
                for px in rgba[(row * w + x0) * 4..(row * w + x1) * 4].chunks_exact(4) {
                    for (s, c) in sum.iter_mut().zip(px) { *s += *c as u32; }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            out.extend(sum.map(|s| (s / n) as u8));
        }
    }
    ([nw, nh], out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(size: [usize; 2], px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take(size[0] * size[1] * 4).collect()
    }

    #[test]
    fn a_large_frame_is_scaled_to_the_longest_side_and_stays_a_readable_png() {
        let img = Image::from_rgba("rack", [2800, 1750], &solid([2800, 1750], [10, 200, 30, 255])).unwrap();
        assert_eq!(img.size, Some([1400, 875]));
        assert_eq!(img.describe(), "rack (1400×875 px)");
        let (size, rgba) = decode_png(&img.bytes).unwrap();
        assert_eq!(size, [1400, 875]);
        assert_eq!(&rgba[..4], &[10, 200, 30, 255], "the colour survives the mean");
        assert!(img.bytes.len() < 20_000, "{} bytes for a flat frame", img.bytes.len());
    }

    #[test]
    fn a_small_image_keeps_its_pixels_and_its_transparency() {
        let mut rgba = solid([3, 2], [255, 0, 0, 255]);
        rgba[3] = 0;
        let img = Image::from_rgba("dot", [3, 2], &rgba).unwrap();
        assert_eq!(decode_png(&img.bytes).unwrap(), ([3, 2], rgba));
    }

    #[test]
    fn scaling_averages_the_pixels_it_merges() {
        // 4x2 → 2x1: black and white columns merge to grey.
        let rgba: Vec<u8> = [[0, 0, 0, 255], [255, 255, 255, 255], [0, 0, 0, 255], [0, 0, 0, 255]].iter().cycle().take(8).flatten().copied().collect();
        let (size, out) = downscale([4, 2], &rgba, 2);
        assert_eq!(size, [2, 1]);
        assert_eq!(out, vec![127, 127, 127, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn a_file_is_read_by_what_it_is_not_by_its_name() {
        let png = Image::from_rgba("x", [2, 2], &solid([2, 2], [1, 2, 3, 255])).unwrap();
        let again = Image::from_file("shot.jpg", &png.bytes).unwrap();
        assert_eq!((again.format, again.size), (Format::Png, Some([2, 2])));
        let jpeg = Image::from_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16]).unwrap();
        assert_eq!((jpeg.format, jpeg.size), (Format::Jpeg, None));
        assert_eq!(jpeg.describe(), "photo.jpg (1 KB)");
        assert!(Image::from_file("notes.txt", b"hello").unwrap_err().contains("neither a PNG nor a JPEG"));
        assert!(Image::from_file("big.jpg", &[[0xFF, 0xD8, 0xFF].as_slice(), &vec![0; MAX_JPEG_BYTES]].concat()).unwrap_err().contains("at most 3 MB"));
        assert!(Image::from_file("cut.png", &png.bytes[..20]).unwrap_err().contains("not a readable PNG"));
    }

    #[test]
    fn the_wire_shapes_carry_the_file_as_base64() {
        let img = Image { name: "n".into(), format: Format::Jpeg, bytes: vec![0xFF, 0xD8, 0xFF].into(), size: None };
        assert_eq!(img.chat_part(), json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/"}}));
        assert_eq!(img.decision_asset(), json!({"type": "image", "format": "jpeg", "data_base64": "/9j/"}));
    }
}
