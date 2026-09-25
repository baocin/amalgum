//! `AMALGUM_SCREENSHOT=<file.png>`: render a few frames, save the window as a PNG, and exit.
//! Lets agents and CI look at the UI without a screenshot tool (e.g. under Xvfb):
//! `AMALGUM_HOME=$(mktemp -d) AMALGUM_SCREENSHOT=shot.png xvfb-run cargo run`.

use std::path::PathBuf;

/// Frames to let layout, fonts, and background jobs settle before capturing.
const SETTLE_FRAMES: u32 = 30;

#[derive(Debug, Default)]
pub struct Screenshot {
    target: Option<PathBuf>,
    frames: u32,
    requested: bool,
}

impl Screenshot {
    pub fn from_env() -> Self {
        let target = std::env::var_os("AMALGUM_SCREENSHOT").filter(|v| !v.is_empty()).map(PathBuf::from);
        Self { target, frames: 0, requested: false }
    }

    /// Call once per frame.
    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(target) = &self.target else { return };
        self.frames += 1;
        if !self.requested && self.frames >= SETTLE_FRAMES {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.requested = true;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [w, h] = image.size;
            let rgba: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
            if let Err(e) = std::fs::write(target, encode_png(w as u32, h as u32, &rgba)) {
                eprintln!("amalgum: could not write screenshot {}: {e}", target.display());
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

/// Minimal RGBA8 PNG: zlib with stored (uncompressed) deflate blocks. Big, but dependency-free.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks(row).take(height as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(line);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr[..]), (b"IDAT", &z[..]), (b"IEND", &[][..])] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }
    png
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn png_has_valid_structure() {
        let png = encode_png(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // IHDR CRC covers the chunk type and data.
        let ihdr_crc = u32::from_be_bytes(png[29..33].try_into().unwrap());
        assert_eq!(ihdr_crc, crc32(&png[12..29]));
    }

    #[test]
    fn large_images_split_into_stored_blocks() {
        let (w, h) = (300u32, 300u32);
        let png = encode_png(w, h, &vec![7; (w * h * 4) as usize]);
        // Raw size exceeds one 64 KiB stored block, so the IDAT must be larger than the data.
        assert!(png.len() > (w * h * 4) as usize);
    }
}
