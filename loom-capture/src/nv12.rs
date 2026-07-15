//! NV12 (biplanar 4:2:0) → I420 (planar 4:2:0).
//!
//! ScreenCaptureKit is configured for `420v`, so the RGB→YUV conversion already
//! happened on the GPU and both planes arrive as BT.601 **video range** — the
//! same range the Linux converter produces, which is why the encoder can be fed
//! identically from either host. All that remains is splitting NV12's one
//! interleaved CbCr plane into the separate U and V planes I420 wants.
//!
//! Source strides are honored: CoreVideo pads rows to its own alignment, so
//! `bytes_per_row` is generally larger than the frame width.

use crate::frame::I420Buffer;

/// Split an NV12 frame into `dst`. `y`/`uv` are the two CoreVideo planes with
/// their respective row strides; `dst`'s dimensions define how much is read.
pub fn to_i420(y: &[u8], y_stride: usize, uv: &[u8], uv_stride: usize, dst: &mut I420Buffer) {
    let (width, height) = dst.dimensions();
    let (width, height) = (width as usize, height as usize);
    let chroma_width = width / 2;

    let [dst_y, dst_u, dst_v] = dst.planes_mut();

    for row in 0..height {
        let src = &y[row * y_stride..][..width];
        dst_y[row * width..][..width].copy_from_slice(src);
    }

    // One interleaved CbCr row per two luma rows; Cb is U, Cr is V.
    for row in 0..height / 2 {
        let src = &uv[row * uv_stride..][..chroma_width * 2];
        let u_row = &mut dst_u[row * chroma_width..][..chroma_width];
        let v_row = &mut dst_v[row * chroma_width..][..chroma_width];

        for i in 0..chroma_width {
            u_row[i] = src[2 * i];
            v_row[i] = src[2 * i + 1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deinterleaves_chroma_and_honors_strides() {
        // 4×2 frame; both planes padded to a stride wider than the frame.
        let y_stride = 6;
        let y: Vec<u8> = vec![
            1, 2, 3, 4, 0, 0, // row 0 + padding
            5, 6, 7, 8, 0, 0, // row 1 + padding
        ];
        let uv_stride = 5;
        let uv: Vec<u8> = vec![10, 20, 11, 21, 0]; // Cb,Cr,Cb,Cr + padding

        let mut dst = I420Buffer::new(4, 2);
        to_i420(&y, y_stride, &uv, uv_stride, &mut dst);

        let [dy, du, dv] = dst.planes();
        assert_eq!(dy, &[1, 2, 3, 4, 5, 6, 7, 8], "luma rows are packed");
        assert_eq!(du, &[10, 11], "U takes the Cb bytes");
        assert_eq!(dv, &[20, 21], "V takes the Cr bytes");
    }
}
