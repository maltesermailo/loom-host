//! The I420 frame buffer handed to the encoder.
//!
//! Deliberately the *same* plane/stride shape the synthetic `TestPattern`
//! exposes (tightly-packed I420, `[Y, U, V]` + `[w, w/2, w/2]`), so `loomd`'s
//! media loop feeds capture and synthetic frames to `encode_i420` identically
//! (spec/PROTOCOL.md §4.1, §5). Storage is one contiguous `Vec<u8>` — Y then U
//! then V — so copying a whole frame in/out of the shared slot is one `memcpy`.

/// A reusable tightly-packed I420 frame (8-bit 4:2:0). `width`/`height` are even.
#[derive(Clone)]
pub struct I420Buffer {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl I420Buffer {
    /// Allocate planes for a `width`×`height` frame (both rounded down to even).
    pub fn new(width: u32, height: u32) -> Self {
        let width = width & !1;
        let height = height & !1;

        let y = width as usize * height as usize;
        let uv = (width as usize / 2) * (height as usize / 2);

        Self {
            width,
            height,
            data: vec![0; y + 2 * uv],
        }
    }

    /// Frame dimensions in pixels.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The Y, U, V plane slices (for `HevcEncoder::encode_i420`).
    pub fn planes(&self) -> [&[u8]; 3] {
        let (y, rest) = self.data.split_at(self.y_len());
        let (u, v) = rest.split_at(self.uv_len());
        [y, u, v]
    }

    /// Luma/chroma strides (tightly packed).
    pub fn strides(&self) -> [i32; 3] {
        [
            self.width as i32,
            (self.width / 2) as i32,
            (self.width / 2) as i32,
        ]
    }

    /// Mutable Y, U, V plane slices (for the converter to write into).
    pub fn planes_mut(&mut self) -> [&mut [u8]; 3] {
        let y_len = self.y_len();
        let uv_len = self.uv_len();

        let (y, rest) = self.data.split_at_mut(y_len);
        let (u, v) = rest.split_at_mut(uv_len);
        [y, u, v]
    }

    /// Copy this frame's pixels into `dst` (same dimensions required).
    pub fn copy_into(&self, dst: &mut I420Buffer) {
        debug_assert_eq!(self.data.len(), dst.data.len());
        dst.data.copy_from_slice(&self.data);
    }

    fn y_len(&self) -> usize {
        self.width as usize * self.height as usize
    }

    fn uv_len(&self) -> usize {
        (self.width as usize / 2) * (self.height as usize / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_dimensions_round_down_and_planes_are_sized() {
        let f = I420Buffer::new(65, 49);
        assert_eq!(f.dimensions(), (64, 48));

        let [y, u, v] = f.planes();
        assert_eq!(y.len(), 64 * 48);
        assert_eq!(u.len(), 32 * 24);
        assert_eq!(v.len(), 32 * 24);
    }

    #[test]
    fn copy_into_roundtrips() {
        let mut src = I420Buffer::new(8, 8);
        src.planes_mut()[0][0] = 42;
        let mut dst = I420Buffer::new(8, 8);
        src.copy_into(&mut dst);
        assert_eq!(dst.planes()[0][0], 42);
    }
}
