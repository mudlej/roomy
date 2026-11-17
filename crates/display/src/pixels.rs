//! Guest screen contents as plain pixels.
//!
//! QEMU describes pixels with pixman format codes and delivers them either
//! inline or as a shared memory mapping. Everything here ends up as tightly
//! packed 32-bit BGRX rows (blue first, fourth byte unused), which is what
//! the window draws.

use std::os::fd::{AsRawFd, OwnedFd};

/// Pixman format codes QEMU uses for guest screens.
pub const PIXMAN_X8R8G8B8: u32 = 0x2002_0888;
pub const PIXMAN_A8R8G8B8: u32 = 0x2002_8888;
pub const PIXMAN_R8G8B8: u32 = 0x1802_0888;
pub const PIXMAN_R5G6B5: u32 = 0x1002_0565;

/// Bytes per pixel of a format, or `None` if roomy cannot draw it.
pub fn bytes_per_pixel(pixman_format: u32) -> Option<usize> {
    match pixman_format {
        PIXMAN_X8R8G8B8 | PIXMAN_A8R8G8B8 => Some(4),
        PIXMAN_R8G8B8 => Some(3),
        PIXMAN_R5G6B5 => Some(2),
        _ => None,
    }
}

/// Why pixels could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PixelError {
    UnsupportedFormat(u32),
    /// The sizes do not describe an area inside the data.
    OutOfBounds,
}

impl std::fmt::Display for PixelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PixelError::UnsupportedFormat(format) => write!(f, "unsupported pixel format {format:#x}"),
            PixelError::OutOfBounds => f.write_str("the guest sent a screen area that does not fit its data"),
        }
    }
}

/// A screen area inside a larger buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Converts one row of `format` pixels into BGRX.
///
/// # Safety
/// `src` must be readable for `width * bytes_per_pixel(format)` bytes.
unsafe fn convert_row(format: u32, src: *const u8, dst: &mut [u8], width: usize) {
    // SAFETY: the caller guarantees `src` covers the row; reads are unaligned byte reads.
    unsafe {
        match format {
            PIXMAN_X8R8G8B8 | PIXMAN_A8R8G8B8 => std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), width * 4),
            PIXMAN_R8G8B8 => {
                for (i, out) in dst.chunks_exact_mut(4).take(width).enumerate() {
                    std::ptr::copy_nonoverlapping(src.add(i * 3), out.as_mut_ptr(), 3);
                    out[3] = 0xff;
                }
            }
            _ => {
                for (i, out) in dst.chunks_exact_mut(4).take(width).enumerate() {
                    let pixel = u16::from_le_bytes([src.add(i * 2).read(), src.add(i * 2 + 1).read()]);
                    let (r, g, b) = ((pixel >> 11) as u8, ((pixel >> 5) & 0x3f) as u8, (pixel & 0x1f) as u8);
                    out.copy_from_slice(&[(b << 3) | (b >> 2), (g << 2) | (g >> 4), (r << 3) | (r >> 2), 0xff]);
                }
            }
        }
    }
}

/// The guest screen as roomy keeps it between updates: BGRX, `width * 4` bytes per row.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Row length in bytes.
    pub fn stride(&self) -> usize {
        self.width * 4
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    /// Replaces the whole screen with `data` (rows of `stride` bytes in `format`).
    pub fn set(
        &mut self,
        width: usize,
        height: usize,
        stride: usize,
        format: u32,
        data: &[u8],
    ) -> Result<(), PixelError> {
        let mut fresh = Canvas { width, height, pixels: vec![0; checked_area(width, height)? * 4] };
        fresh.draw(Rect { x: 0, y: 0, width, height }, stride, format, data)?;
        *self = fresh;
        Ok(())
    }

    /// Overwrites `area` with `data`, whose first pixel is the area's top-left corner.
    pub fn draw(&mut self, area: Rect, stride: usize, format: u32, data: &[u8]) -> Result<(), PixelError> {
        let bpp = bytes_per_pixel(format).ok_or(PixelError::UnsupportedFormat(format))?;
        self.check(area)?;
        if area.width == 0 || area.height == 0 {
            return Ok(());
        }
        let needed = stride
            .checked_mul(area.height - 1)
            .and_then(|n| n.checked_add(area.width.checked_mul(bpp)?))
            .ok_or(PixelError::OutOfBounds)?;
        if data.len() < needed || stride < area.width * bpp {
            return Err(PixelError::OutOfBounds);
        }
        for row in 0..area.height {
            let start = ((area.y + row) * self.width + area.x) * 4;
            // SAFETY: `needed <= data.len()` covers every row read here.
            unsafe {
                convert_row(
                    format,
                    data.as_ptr().add(row * stride),
                    &mut self.pixels[start..start + area.width * 4],
                    area.width,
                )
            };
        }
        Ok(())
    }

    /// Refreshes `area` (or everything, if `None`) from shared memory. A
    /// mapping of another size replaces the canvas.
    pub fn draw_mapped(&mut self, map: &Mapping, area: Option<Rect>) -> Result<(), PixelError> {
        let bpp = bytes_per_pixel(map.format).ok_or(PixelError::UnsupportedFormat(map.format))?;
        if (self.width, self.height) != (map.width, map.height) {
            *self = Canvas {
                width: map.width,
                height: map.height,
                pixels: vec![0; checked_area(map.width, map.height)? * 4],
            };
        }
        let area = match area {
            Some(area) if (area.width, area.height) != (0, 0) => area,
            _ => Rect { x: 0, y: 0, width: self.width, height: self.height },
        };
        self.check(area)?;
        for row in 0..area.height {
            let start = ((area.y + row) * self.width + area.x) * 4;
            // SAFETY: Mapping::new checked that every row of the frame lies inside the mapping,
            // and `check` that the area lies inside the frame.
            unsafe {
                let src = map.frame().add((area.y + row) * map.stride + area.x * bpp);
                convert_row(map.format, src, &mut self.pixels[start..start + area.width * 4], area.width);
            }
        }
        Ok(())
    }

    fn check(&self, area: Rect) -> Result<(), PixelError> {
        let fits = |start: usize, len: usize, limit: usize| start.checked_add(len).is_some_and(|end| end <= limit);
        if fits(area.x, area.width, self.width) && fits(area.y, area.height, self.height) {
            Ok(())
        } else {
            Err(PixelError::OutOfBounds)
        }
    }
}

/// Screens larger than this are refused rather than allocated.
const MAX_PIXELS: usize = 16_384 * 16_384;

fn checked_area(width: usize, height: usize) -> Result<usize, PixelError> {
    width.checked_mul(height).filter(|area| *area <= MAX_PIXELS).ok_or(PixelError::OutOfBounds)
}

/// Guest screen memory shared by QEMU. QEMU keeps drawing into it; roomy
/// only ever copies out of it.
#[derive(Debug)]
pub struct Mapping {
    base: *mut libc::c_void,
    len: usize,
    offset: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
}

// SAFETY: the mapping is read-only memory that stays valid until drop; it is only read through raw copies.
unsafe impl Send for Mapping {}
// SAFETY: as above; there is no interior state.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps the frame QEMU described. Fails if the description does not fit
    /// the shared memory, so later copies can never run past its end.
    pub fn new(
        fd: OwnedFd,
        offset: usize,
        width: usize,
        height: usize,
        stride: usize,
        format: u32,
    ) -> Result<Mapping, String> {
        let bpp = bytes_per_pixel(format).ok_or_else(|| PixelError::UnsupportedFormat(format).to_string())?;
        checked_area(width, height).map_err(|e| e.to_string())?;
        let frame = stride.checked_mul(height).filter(|_| width > 0 && height > 0 && stride >= width * bpp);
        let len =
            frame.and_then(|frame| frame.checked_add(offset)).ok_or("the shared screen has an impossible size")?;
        // SAFETY: fstat on a descriptor we own, into a zeroed stat buffer.
        let size = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd.as_raw_fd(), &mut stat) != 0 {
                return Err(format!("cannot inspect the shared screen: {}", std::io::Error::last_os_error()));
            }
            stat.st_size as u64
        };
        if size < len as u64 {
            return Err(format!(
                "the shared screen is smaller ({size} bytes) than the frame it should hold ({len} bytes)"
            ));
        }
        // SAFETY: a fresh read-only shared mapping of a descriptor we own; the length was checked against the file.
        let base =
            unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if base == libc::MAP_FAILED {
            return Err(format!("cannot map the shared screen: {}", std::io::Error::last_os_error()));
        }
        Ok(Mapping { base, len, offset, width, height, stride, format })
    }

    fn frame(&self) -> *const u8 {
        // SAFETY: offset < len, checked in `new`.
        unsafe { self.base.cast::<u8>().add(self.offset) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `new` mapped.
        unsafe { libc::munmap(self.base, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_32_bit_screen_is_copied_row_by_row_ignoring_padding() {
        let mut canvas = Canvas::default();
        // 2x2 pixels, rows padded to 12 bytes.
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 0xee, 0xee, 0xee, 0xee, 9, 10, 11, 12, 13, 14, 15, 16];
        canvas.set(2, 2, 12, PIXMAN_X8R8G8B8, &data).unwrap();
        assert_eq!((canvas.width(), canvas.height(), canvas.stride()), (2, 2, 8));
        assert_eq!(canvas.pixels(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn an_update_changes_only_its_area() {
        let mut canvas = Canvas::default();
        canvas.set(3, 2, 12, PIXMAN_X8R8G8B8, &[0; 24]).unwrap();
        canvas.draw(Rect { x: 1, y: 1, width: 2, height: 1 }, 8, PIXMAN_A8R8G8B8, &[1, 1, 1, 1, 2, 2, 2, 2]).unwrap();
        assert_eq!(&canvas.pixels()[..16], &[0; 16]);
        assert_eq!(&canvas.pixels()[16..], &[1, 1, 1, 1, 2, 2, 2, 2]);
    }

    #[test]
    fn sixteen_and_twenty_four_bit_screens_are_converted() {
        let mut canvas = Canvas::default();
        // Pure red, then pure blue, in r5g6b5 (little endian).
        canvas.set(2, 1, 4, PIXMAN_R5G6B5, &[0x00, 0xf8, 0x1f, 0x00]).unwrap();
        assert_eq!(canvas.pixels(), &[0, 0, 0xff, 0xff, 0xff, 0, 0, 0xff]);
        // r8g8b8 stores blue first, like the 32-bit formats.
        canvas.set(1, 1, 3, PIXMAN_R8G8B8, &[10, 20, 30]).unwrap();
        assert_eq!(canvas.pixels(), &[10, 20, 30, 0xff]);
    }

    #[test]
    fn data_that_does_not_fit_is_refused_and_leaves_the_screen_alone() {
        let mut canvas = Canvas::default();
        canvas.set(2, 2, 8, PIXMAN_X8R8G8B8, &[7; 16]).unwrap();
        let before = canvas.clone();
        assert_eq!(canvas.set(2, 2, 8, PIXMAN_X8R8G8B8, &[0; 15]), Err(PixelError::OutOfBounds));
        assert_eq!(
            canvas.set(2, 2, 4, PIXMAN_X8R8G8B8, &[0; 16]),
            Err(PixelError::OutOfBounds),
            "stride shorter than a row"
        );
        assert_eq!(canvas.set(usize::MAX, 2, 8, PIXMAN_X8R8G8B8, &[0; 16]), Err(PixelError::OutOfBounds));
        assert_eq!(canvas.set(2, 2, 8, 0x1234, &[0; 16]), Err(PixelError::UnsupportedFormat(0x1234)));
        let outside = Rect { x: 1, y: 0, width: 2, height: 1 };
        assert_eq!(canvas.draw(outside, 8, PIXMAN_X8R8G8B8, &[0; 8]), Err(PixelError::OutOfBounds));
        let overflow = Rect { x: usize::MAX, y: 0, width: 2, height: 1 };
        assert_eq!(canvas.draw(overflow, 8, PIXMAN_X8R8G8B8, &[0; 8]), Err(PixelError::OutOfBounds));
        assert_eq!(canvas, before);
    }

    fn shared_memory(content: &[u8]) -> OwnedFd {
        use std::os::fd::FromRawFd;
        // SAFETY: memfd_create returns a new descriptor that we take ownership of.
        let fd = unsafe { libc::memfd_create(c"roomy-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: fd is valid and unowned.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(content).unwrap();
        file.into()
    }

    #[test]
    fn a_shared_screen_is_read_at_its_offset_and_follows_later_changes() {
        // 4 bytes of something else, then a 2x1 frame.
        let fd = shared_memory(&[0xaa, 0xaa, 0xaa, 0xaa, 1, 2, 3, 4, 5, 6, 7, 8]);
        let mut writer = std::fs::File::from(fd.try_clone().unwrap());
        let map = Mapping::new(fd, 4, 2, 1, 8, PIXMAN_X8R8G8B8).unwrap();
        let mut canvas = Canvas::default();
        canvas.draw_mapped(&map, None).unwrap();
        assert_eq!(canvas.pixels(), &[1, 2, 3, 4, 5, 6, 7, 8]);

        // QEMU draws into the same memory; an update of the second pixel picks it up.
        use std::os::unix::fs::FileExt;
        writer.write_all_at(&[9, 9, 9, 9, 8, 8, 8, 8], 4).unwrap();
        writer.flush().unwrap();
        canvas.draw_mapped(&map, Some(Rect { x: 1, y: 0, width: 1, height: 1 })).unwrap();
        assert_eq!(canvas.pixels(), &[1, 2, 3, 4, 8, 8, 8, 8]);
        assert_eq!(
            canvas.draw_mapped(&map, Some(Rect { x: 2, y: 0, width: 1, height: 1 })),
            Err(PixelError::OutOfBounds)
        );
    }

    #[test]
    fn a_frame_larger_than_the_shared_memory_is_refused() {
        let error = Mapping::new(shared_memory(&[0; 16]), 0, 2, 3, 8, PIXMAN_X8R8G8B8).unwrap_err();
        assert!(error.contains("smaller"), "{error}");
        assert!(Mapping::new(shared_memory(&[0; 16]), 0, 0, 0, 0, PIXMAN_X8R8G8B8).is_err());
        assert!(Mapping::new(shared_memory(&[0; 16]), usize::MAX, 1, 1, 4, PIXMAN_X8R8G8B8).is_err());
    }
}
