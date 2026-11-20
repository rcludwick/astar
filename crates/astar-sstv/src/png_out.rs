// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Saving a decoded picture as a PNG.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::Path;

use crate::Picture;

impl Picture {
    /// Write the picture as an 8-bit RGB PNG at `path`. The file must not
    /// exist yet: an existing file is never overwritten, and the error's
    /// kind is then [`io::ErrorKind::AlreadyExists`].
    pub fn write_png(&self, path: &Path) -> io::Result<()> {
        let expected = self.width as usize * self.height as usize * 3;
        if self.width == 0 || self.height == 0 || self.rgb.len() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "picture is {}x{} but carries {} bytes",
                    self.width,
                    self.height,
                    self.rgb.len()
                ),
            ));
        }
        // `create_new`: past this line the file is ours, so a failure below
        // may remove it. Before it, nothing here touches the path.
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = self.encode(file);
        if result.is_err() {
            // A half-written file is no picture.
            let _ = std::fs::remove_file(path);
        }
        result
    }

    fn encode(&self, file: File) -> io::Result<()> {
        let mut encoder = png::Encoder::new(BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(io::Error::other)?;
        writer
            .write_image_data(&self.rgb)
            .map_err(io::Error::other)?;
        writer.finish().map_err(io::Error::other)
    }
}
