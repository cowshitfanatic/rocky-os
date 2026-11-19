//! Text output on the bootloader-provided pixel framebuffer.
//!
//! The BIOS bootloader sets up a graphics mode (1280x720 in QEMU), so the
//! classic VGA text buffer at 0xb8000 is *not* usable here. Instead we
//! render pre-rasterized glyphs from the Noto Sans Mono bitmap font
//! directly into the framebuffer pixels.

use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use core::fmt;
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster};

/// Global framebuffer writer, set up once in `kernel_main`.
pub static FRAMEBUFFER: spin::Mutex<Option<FrameBufferWriter>> = spin::Mutex::new(None);

/// Screen size, cached in atomics at init so interrupt handlers
/// can read it without taking the FRAMEBUFFER lock — an IRQ
/// handler must never spin on a lock the interrupted code may
/// hold (that deadlocks the CPU silently, with interrupts off).
pub static FB_WIDTH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub static FB_HEIGHT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn init(framebuffer: FrameBuffer) {
    let info = framebuffer.info();
    crate::serial_print_try(format_args!(
        "fb: {}x{} stride={} bpp={} format={:?}\n",
        info.width, info.height, info.stride, info.bytes_per_pixel, info.pixel_format
    ));
    FB_WIDTH.store(info.width, core::sync::atomic::Ordering::SeqCst);
    FB_HEIGHT.store(info.height, core::sync::atomic::Ordering::SeqCst);
    let buffer = framebuffer.into_buffer();
    let mut writer = FrameBufferWriter::new(buffer, info);
    writer.clear();
    *FRAMEBUFFER.lock() = Some(writer);
}

/// Clear the framebuffer if it was initialised.
pub fn clear() {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        writer.clear();
    }
}

/// Print to the framebuffer if it was initialised; silently ignore otherwise.
pub fn print(args: fmt::Arguments) {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        use fmt::Write;
        let _ = writer.write_fmt(args);
    }
}

/// An RGB colour (red, green, blue), each 0..=255.
pub type Color = (u8, u8, u8);

pub fn width() -> usize {
    FB_WIDTH.load(core::sync::atomic::Ordering::SeqCst)
}

pub fn height() -> usize {
    FB_HEIGHT.load(core::sync::atomic::Ordering::SeqCst)
}

pub fn set_pixel(x: usize, y: usize, color: Color) {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        writer.set_pixel(x, y, color);
    }
}

pub fn fill_rect(x: usize, y: usize, w: usize, h: usize, color: Color) {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        writer.fill_rect(x, y, w, h, color);
    }
}

pub fn draw_rect(x: usize, y: usize, w: usize, h: usize, color: Color) {
    fill_rect(x, y, w, 1, color);
    fill_rect(x, y + h.saturating_sub(1), w, 1, color);
    fill_rect(x, y, 1, h, color);
    fill_rect(x + w.saturating_sub(1), y, 1, h, color);
}

/// Draw text at a pixel position. `bg` of `None` leaves the
/// background untouched (transparent); `Some` fills each cell.
pub fn draw_text(x: usize, y: usize, text: &str, fg: Color, bg: Option<Color>) {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        writer.draw_text(x, y, text, fg, bg);
    }
}

/// Draw text scaled up by `scale` (each glyph pixel becomes a
/// `scale` x `scale` block). Used for the large clock in the
/// desktop widget.
pub fn draw_text_scaled(x: usize, y: usize, text: &str, fg: Color, bg: Option<Color>, scale: usize) {
    if let Some(writer) = FRAMEBUFFER.lock().as_mut() {
        writer.draw_text_scaled(x, y, text, fg, bg, scale);
    }
}

/// Integer square root (Newton's method, u32 arithmetic).
pub fn isqrt(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Fill a circle centred at (`cx`, `cy`) with radius `r`,
/// as one `fill_rect` per row (fast row copies, no per-pixel loop).
pub fn fill_circle(cx: i32, cy: i32, r: i32, color: Color) {
    if r < 0 {
        return;
    }
    for dy in -r..=r {
        let dx = isqrt((r * r - dy * dy).max(0) as u32) as i32;
        let x0 = (cx - dx).max(0);
        let x1 = cx + dx;
        if x1 < 0 {
            continue;
        }
        let y = cy + dy;
        if y < 0 {
            continue;
        }
        fill_rect(x0 as usize, y as usize, (x1 - x0 + 1).max(0) as usize, 1, color);
    }
}

/// Fill a rectangle with rounded corners of radius `r`, as
/// horizontal bands (the corner rows are inset following the
/// circle). `r` is clamped to half the smaller side.
pub fn fill_rounded_rect(x: usize, y: usize, w: usize, h: usize, r: usize, color: Color) {
    let r = r.min(w / 2).min(h / 2);
    if r == 0 {
        fill_rect(x, y, w, h, color);
        return;
    }
    if h > 2 * r {
        fill_rect(x, y + r, w, h - 2 * r, color);
    }
    let r32 = r as u32;
    for i in 0..r {
        // Row i from the top edge: the corner circle's centre is
        // at row r, so the vertical distance is r - i.
        let dy = r32 - i as u32;
        let dx = isqrt(r32 * r32 - dy * dy) as usize;
        let inset = r - dx;
        let row_w = w.saturating_sub(2 * inset);
        fill_rect(x + inset, y + i, row_w, 1, color);
        fill_rect(x + inset, y + h - 1 - i, row_w, 1, color);
    }
}

#[macro_export]
macro_rules! fb_print {
    ($($arg:tt)*) => ($crate::framebuffer::print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! fb_println {
    () => ($crate::fb_print!("\n"));
    ($($arg:tt)*) => ($crate::fb_print!("{}\n", format_args!($($arg)*)));
}

pub struct FrameBufferWriter {
    framebuffer: &'static mut [u8],
    info: FrameBufferInfo,
    x_pos: usize,
    y_pos: usize,
}

impl FrameBufferWriter {
    pub fn new(framebuffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        Self {
            framebuffer,
            info,
            x_pos: 0,
            y_pos: 0,
        }
    }

    fn newline(&mut self) {
        self.y_pos += 16;
        self.x_pos = 0;
        // Scroll: clear and restart at the top if we run out of rows.
        if self.y_pos + 16 > self.info.height {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.x_pos = 0;
        self.y_pos = 0;
        self.framebuffer.fill(0);
    }

    pub fn width(&self) -> usize {
        self.info.width
    }

    pub fn height(&self) -> usize {
        self.info.height
    }

    fn color_bytes(&self, color: Color) -> [u8; 3] {
        match self.info.pixel_format {
            PixelFormat::Rgb => [color.0, color.1, color.2],
            // QEMU's bootloader framebuffer is usually BGR.
            _ => [color.2, color.1, color.0],
        }
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: Color) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let bytes = self.color_bytes(color);
        let bpp = self.info.bytes_per_pixel;
        let offset = (y * self.info.stride + x) * bpp;
        if offset + bpp > self.framebuffer.len() {
            return;
        }
        self.framebuffer[offset..offset + bpp].copy_from_slice(&bytes[..bpp]);
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let x_end = (x + w).min(self.info.width);
        let y_end = (y + h).min(self.info.height);
        if x_end <= x || y_end <= y {
            return;
        }
        // Build one row of the colour once, then memcpy it into
        // each row. (A per-pixel loop here costs ~1.5 s per full
        // desktop redraw in a debug build; the row copy is ~1 ms.)
        // The row buffer is MaybeUninit: zeroing 8 KiB per call
        // (there are ~1500 calls per desktop frame) is pure waste,
        // since every byte we read is written first below.
        let bpp = self.info.bytes_per_pixel;
        let bytes = self.color_bytes(color);
        let row_bytes = (x_end - x) * bpp;
        let mut rowbuf = core::mem::MaybeUninit::<[u8; 8192]>::uninit();
        if row_bytes > 8192 {
            // Extremely wide rect: fall back to per-pixel.
            for yy in y..y_end {
                for xx in x..x_end {
                    self.set_pixel(xx, yy, color);
                }
            }
            return;
        }
        let rowbuf = unsafe {
            let ptr = rowbuf.as_mut_ptr() as *mut u8;
            // row_bytes is always a multiple of bpp, so copy the
            // 3-or-4-byte colour pattern in whole chunks: the old
            // per-byte `bytes[i % bpp]` loop paid an integer
            // division per byte (~135 us per small fill_rect call
            // in a debug build, dwarfing the actual pixel writes).
            let mut i = 0;
            while i < row_bytes {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(i), bpp);
                i += bpp;
            }
            core::slice::from_raw_parts(ptr, row_bytes)
        };
        for yy in y..y_end {
            let start = (yy * self.info.stride + x) * bpp;
            self.framebuffer[start..start + row_bytes].copy_from_slice(rowbuf);
        }
    }

    /// Draw one glyph cell (8x16) at a pixel position, blending
    /// the foreground over the background by glyph intensity.
    /// The colour bytes are computed once per glyph (not per
    /// pixel), and a fully on-screen glyph skips the per-pixel
    /// bounds checks and colour conversion: text is ~90k pixels
    /// per desktop frame, so the per-pixel `color_bytes` match
    /// and bounds checks cost real milliseconds in a debug build.
    /// (Blending the pre-converted framebuffer bytes is correct:
    /// fg and bg are both in framebuffer byte order, so each
    /// channel blends with its own counterpart.)
    fn draw_glyph(&mut self, x: usize, y: usize, c: char, fg: Color, bg: Option<Color>) {
        let raster = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
            .unwrap_or_else(|| get_raster(' ', FontWeight::Regular, RasterHeight::Size16).unwrap());
        let rows = raster.raster();
        let fg_bytes = self.color_bytes(fg);
        let bg_bytes = bg.map(|b| self.color_bytes(b));
        let bpp = self.info.bytes_per_pixel;
        let on_screen = x + 8 <= self.info.width && y + 16 <= self.info.height;
        for (dy, row) in rows.iter().enumerate() {
            for (dx, &intensity) in row.iter().enumerate() {
                if intensity == 0 {
                    match bg {
                        Some(bgc) => {
                            if on_screen {
                                self.write_pixel_bytes(x + dx, y + dy, bg_bytes.unwrap(), bpp);
                            } else {
                                self.set_pixel(x + dx, y + dy, bgc);
                            }
                        }
                        None => {}
                    }
                    continue;
                }
                let t = intensity as u16;
                if on_screen {
                    let bytes = match bg_bytes {
                        Some(bb) => {
                            let mut out = [0u8; 3];
                            for i in 0..bpp {
                                out[i] = ((fg_bytes[i] as u16 * t + bb[i] as u16 * (255 - t)) / 255) as u8;
                            }
                            out
                        }
                        None => fg_bytes,
                    };
                    self.write_pixel_bytes(x + dx, y + dy, bytes, bpp);
                } else {
                    // Rare off-screen fallback: recompute the RGB
                    // colour and go through the checked path.
                    let rgb = match bg {
                        Some(bgc) => (
                            ((fg.0 as u16 * t + bgc.0 as u16 * (255 - t)) / 255) as u8,
                            ((fg.1 as u16 * t + bgc.1 as u16 * (255 - t)) / 255) as u8,
                            ((fg.2 as u16 * t + bgc.2 as u16 * (255 - t)) / 255) as u8,
                        ),
                        None => fg,
                    };
                    self.set_pixel(x + dx, y + dy, rgb);
                }
            }
        }
    }

    /// Write one pixel's bytes with no bounds check and no colour
    /// conversion (caller guarantees the pixel is on screen).
    fn write_pixel_bytes(&mut self, x: usize, y: usize, bytes: [u8; 3], bpp: usize) {
        let offset = (y * self.info.stride + x) * bpp;
        self.framebuffer[offset..offset + bpp].copy_from_slice(&bytes[..bpp]);
    }

    pub fn draw_text(&mut self, x: usize, y: usize, text: &str, fg: Color, bg: Option<Color>) {
        let mut cx = x;
        for c in text.chars() {
            if cx + 8 > self.info.width {
                break;
            }
            self.draw_glyph(cx, y, c, fg, bg);
            cx += 8;
        }
    }

    /// Draw text with each glyph pixel scaled to `scale` x `scale`
    /// screen pixels, writing straight into the buffer (one lock
    /// for the whole string, not one per pixel).
    pub fn draw_text_scaled(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        fg: Color,
        bg: Option<Color>,
        scale: usize,
    ) {
        let scale = scale.max(1);
        let mut cx = x;
        for c in text.chars() {
            let raster = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
                .unwrap_or_else(|| get_raster(' ', FontWeight::Regular, RasterHeight::Size16).unwrap());
            for (dy, row) in raster.raster().iter().enumerate() {
                for (dx, &intensity) in row.iter().enumerate() {
                    let color = if intensity == 0 {
                        match bg {
                            Some(bg) => (bg.0 as u16, bg.1 as u16, bg.2 as u16),
                            None => continue,
                        }
                    } else {
                        let t = intensity as u16;
                        match bg {
                            Some(bg) => (
                                (fg.0 as u16 * t + bg.0 as u16 * (255 - t)) / 255,
                                (fg.1 as u16 * t + bg.1 as u16 * (255 - t)) / 255,
                                (fg.2 as u16 * t + bg.2 as u16 * (255 - t)) / 255,
                            ),
                            None => (fg.0 as u16, fg.1 as u16, fg.2 as u16),
                        }
                    };
                    let (r, g, b) = (color.0 as u8, color.1 as u8, color.2 as u8);
                    for sy in 0..scale {
                        for sx in 0..scale {
                            self.set_pixel(cx + dx * scale + sx, y + dy * scale + sy, (r, g, b));
                        }
                    }
                }
            }
            cx += 8 * scale;
        }
    }

    fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.x_pos = 0,
            '\u{8}' => {
                // Backspace: erase the previous cell.
                if self.x_pos >= 8 {
                    self.x_pos -= 8;
                    let raster = get_raster(' ', FontWeight::Regular, RasterHeight::Size16)
                        .unwrap();
                    self.write_rendered_char(raster);
                }
            }
            c => {
                let new_x = self.x_pos + 8;
                if new_x > self.info.width {
                    self.newline();
                }
                let raster = get_raster(c, FontWeight::Regular, RasterHeight::Size16)
                    .unwrap_or_else(|| {
                        get_raster(' ', FontWeight::Regular, RasterHeight::Size16).unwrap()
                    });
                self.write_rendered_char(raster);
                self.x_pos += 8;
            }
        }
    }

    fn write_rendered_char(&mut self, raster: noto_sans_mono_bitmap::RasterizedChar) {
        for (y, row) in raster.raster().iter().enumerate() {
            for (x, &intensity) in row.iter().enumerate() {
                self.write_pixel(self.x_pos + x, self.y_pos + y, intensity);
            }
        }
    }

    fn write_pixel(&mut self, x: usize, y: usize, intensity: u8) {
        let pixel_offset = y * self.info.stride + x;
        let color = match self.info.pixel_format {
            PixelFormat::Rgb => [intensity, intensity, intensity],
            PixelFormat::Bgr => [intensity, intensity, intensity],
            PixelFormat::U8 => [intensity, intensity, intensity],
            PixelFormat::Unknown { .. } => [intensity, intensity, intensity],
            _ => [intensity, intensity, intensity],
        };
        let bytes_per_pixel = self.info.bytes_per_pixel;
        let byte_offset = pixel_offset * bytes_per_pixel;
        if byte_offset + bytes_per_pixel > self.framebuffer.len() {
            return;
        }
        self.framebuffer[byte_offset..byte_offset + bytes_per_pixel]
            .copy_from_slice(&color[..bytes_per_pixel]);
        // For Bgr/Rgb the grey value is the same in every channel, so the
        // order does not matter for white-on-black text.
    }
}

impl fmt::Write for FrameBufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}
