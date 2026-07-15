//! Packed 32-bit RGB(x) → I420 conversion (BT.601 limited range).
//!
//! The portal delivers SHM frames as one of four packed 4-byte formats (§ R2
//! verdict: BGRx/BGRA on this stack, but the offer lists all four). The encoder
//! wants planar I420, so this is the mandated copy the SHM path costs (+2–3 ms,
//! ARCHITECTURE §13). Scalar and dependency-free by KISS; if it ever threatens
//! the M1.4 <10 ms capture→encode handoff, it is the first thing to SIMD or move
//! onto the GPU alongside the deferred dmabuf zero-copy path — TODO(R2).

use crate::frame::I420Buffer;

/// A packed 32-bit pixel layout, identified by the byte offsets of R, G, B.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    r: usize,
    g: usize,
    b: usize,
}

impl PixelFormat {
    /// B, G, R, (A/x) memory order — the formats KDE's portal fixates here.
    pub const BGRX: PixelFormat = PixelFormat { r: 2, g: 1, b: 0 };
    /// R, G, B, (A/x) memory order.
    pub const RGBX: PixelFormat = PixelFormat { r: 0, g: 1, b: 2 };
}

/// Convert one packed frame into `dst` (an I420 buffer of the same dimensions).
/// `src_stride` is the source row stride in bytes (portal buffers are padded, so
/// it is read from the PipeWire chunk, never assumed to equal `width*4`).
pub fn to_i420(src: &[u8], src_stride: usize, fmt: PixelFormat, dst: &mut I420Buffer) {
    let (w, h) = dst.dimensions();
    let (w, h) = (w as usize, h as usize);

    let [y_plane, u_plane, v_plane] = dst.planes_mut();

    // Luma: one output sample per source pixel.
    for row in 0..h {
        let src_row = &src[row * src_stride..];
        let y_row = &mut y_plane[row * w..row * w + w];
        for col in 0..w {
            let px = &src_row[col * 4..col * 4 + 4];
            y_row[col] = luma(px[fmt.r] as i32, px[fmt.g] as i32, px[fmt.b] as i32);
        }
    }

    // Chroma: average each 2×2 block so subsampling doesn't drop half the color.
    let (cw, ch) = (w / 2, h / 2);
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                let src_row = &src[(cy * 2 + dy) * src_stride..];
                for dx in 0..2 {
                    let px = &src_row[(cx * 2 + dx) * 4..(cx * 2 + dx) * 4 + 4];
                    r += px[fmt.r] as i32;
                    g += px[fmt.g] as i32;
                    b += px[fmt.b] as i32;
                }
            }
            let (r, g, b) = (r / 4, g / 4, b / 4);
            u_plane[cy * cw + cx] = chroma_u(r, g, b);
            v_plane[cy * cw + cx] = chroma_v(r, g, b);
        }
    }
}

fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

fn chroma_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8
}

fn chroma_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take(w * h * 4).collect()
    }

    #[test]
    fn white_maps_to_full_luma_neutral_chroma() {
        let mut dst = I420Buffer::new(4, 4);
        let src = solid(4, 4, [255, 255, 255, 255]);
        to_i420(&src, 4 * 4, PixelFormat::BGRX, &mut dst);

        let [y, u, v] = dst.planes();
        assert_eq!(y[0], 235); // 16..235 limited-range white
        assert_eq!(u[0], 128);
        assert_eq!(v[0], 128);
    }

    #[test]
    fn byte_order_distinguishes_red_from_blue() {
        // A pure-red pixel in BGRx memory order is [0,0,255,255].
        let mut dst = I420Buffer::new(2, 2);
        let src = solid(2, 2, [0, 0, 255, 255]);
        to_i420(&src, 2 * 4, PixelFormat::BGRX, &mut dst);
        let v_red = dst.planes()[2][0];

        // The same bytes read as RGBx are pure blue — V must differ.
        let mut dst2 = I420Buffer::new(2, 2);
        to_i420(&src, 2 * 4, PixelFormat::RGBX, &mut dst2);
        let v_blue = dst2.planes()[2][0];

        assert!(v_red > 200, "red → high V, got {v_red}");
        assert!(v_blue < 128, "blue → low V, got {v_blue}");
    }

    #[test]
    fn honors_padded_source_stride() {
        // 2×2 image with 4 bytes of row padding; second row must be found via stride.
        let w = 2;
        let stride = w * 4 + 4;
        let mut src = vec![0u8; stride * 2];
        // Row 1 white, row 0 black — proves we didn't read past/short a row.
        for x in 0..w {
            src[stride + x * 4..stride + x * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        let mut dst = I420Buffer::new(2, 2);
        to_i420(&src, stride, PixelFormat::BGRX, &mut dst);

        let y = dst.planes()[0];
        assert_eq!(y[0], 16, "row 0 black");
        assert_eq!(y[2], 235, "row 1 white");
    }
}
