//! Synthetic test-pattern source (M1.2) — the stand-in for real capture until
//! M1.4. Produces I420 frames carrying three things a human (or a tearing check)
//! can read at a glance:
//!   * a **moving gradient** (luma ramps with x+y, scrolling each frame),
//!   * a **burned-in frame counter** (top-left digits) so regressions/tears are
//!     visible without tooling,
//!   * a **1px frame-parity border** (white on even frames, black on odd) — if a
//!     displayed frame is torn, its top and bottom borders won't match.
//!
//! The automated M1.2 checks key off `frame_seq` in the datagram header and the
//! structured logs, not off these pixels; the burned-in marks are the human aid.

/// 3×5 bitfont for digits 0–9. Each row's low 3 bits are pixels (bit2 = left).
const FONT: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

const GLYPH_SCALE: usize = 8; // each font pixel → 8×8 luma block
const MARGIN: usize = 8;

/// A reusable I420 frame buffer that renders the test pattern in place.
pub struct TestPattern {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl TestPattern {
    /// Allocate planes for a `width`×`height` I420 frame (both even).
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width / 2;
        let ch = height / 2;
        Self {
            width,
            height,
            y: vec![0; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    /// Render frame `n`: gradient + counter + parity border, into the planes.
    pub fn render(&mut self, n: u32) {
        let (w, h) = (self.width, self.height);
        let shift = n.wrapping_mul(3);
        // Moving luma gradient; gentle chroma drift so motion is visible.
        for yy in 0..h {
            for xx in 0..w {
                self.y[yy * w + xx] = (xx as u32 + yy as u32 + shift) as u8;
            }
        }
        let (cw, ch) = (w / 2, h / 2);
        for cy in 0..ch {
            for cx in 0..cw {
                self.u[cy * cw + cx] = (128 + (cx as u32).wrapping_add(shift) / 4) as u8;
                self.v[cy * cw + cx] = (128 + (cy as u32).wrapping_sub(shift) / 4) as u8;
            }
        }
        self.draw_counter(n);
        self.draw_parity_border(n);
    }

    /// The Y, U, V plane slices (for the encoder).
    pub fn planes(&self) -> [&[u8]; 3] {
        [&self.y, &self.u, &self.v]
    }

    /// Luma/chroma strides (tightly packed I420).
    pub fn strides(&self) -> [i32; 3] {
        [
            self.width as i32,
            (self.width / 2) as i32,
            (self.width / 2) as i32,
        ]
    }

    fn draw_counter(&mut self, n: u32) {
        let digits: Vec<u32> = {
            let s = n.to_string();
            s.bytes().map(|b| (b - b'0') as u32).collect()
        };
        let glyph_w = 3 * GLYPH_SCALE + GLYPH_SCALE; // 3px glyph + 1px spacing
        for (i, &d) in digits.iter().enumerate() {
            let ox = MARGIN + i * glyph_w;
            self.draw_glyph(d as usize, ox, MARGIN);
        }
    }

    fn draw_glyph(&mut self, digit: usize, ox: usize, oy: usize) {
        let rows = &FONT[digit];
        for (ry, bits) in rows.iter().enumerate() {
            for rx in 0..3 {
                let lit = (bits >> (2 - rx)) & 1 == 1;
                let val = if lit { 235 } else { 16 }; // white on black
                for dy in 0..GLYPH_SCALE {
                    for dx in 0..GLYPH_SCALE {
                        let x = ox + rx * GLYPH_SCALE + dx;
                        let y = oy + ry * GLYPH_SCALE + dy;
                        if x < self.width && y < self.height {
                            self.y[y * self.width + x] = val;
                        }
                    }
                }
            }
        }
    }

    fn draw_parity_border(&mut self, n: u32) {
        let (w, h) = (self.width, self.height);
        let c = if n.is_multiple_of(2) { 235 } else { 16 };
        for x in 0..w {
            self.y[x] = c; // top row
            self.y[(h - 1) * w + x] = c; // bottom row
        }
        for y in 0..h {
            self.y[y * w] = c; // left col
            self.y[y * w + (w - 1)] = c; // right col
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fills_and_moves() {
        let mut p = TestPattern::new(320, 240);
        p.render(0);
        let f0: Vec<u8> = p.planes()[0].to_vec();
        p.render(1);
        let f1 = p.planes()[0];
        // A non-border interior pixel must change frame-to-frame (gradient moved).
        let idx = 100 * 320 + 100;
        assert_ne!(f0[idx], f1[idx]);
    }

    #[test]
    fn parity_border_flips() {
        let mut p = TestPattern::new(64, 48);
        p.render(0);
        assert_eq!(p.planes()[0][0], 235); // even → white corner
        p.render(1);
        assert_eq!(p.planes()[0][0], 16); // odd → black corner
    }
}
