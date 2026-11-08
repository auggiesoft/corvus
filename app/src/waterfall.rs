//! Waterfall history as a GPU texture ring: one row uploaded per pass, the full texture only when
//! the levels or palette change, or when a new plan re-maps the history onto its frequency axis.

use egui::{Color32, ColorImage, Mesh, Painter, Rect, Shape, TextureHandle, TextureOptions, pos2};

use crate::theme::{self, Palette};

pub const ROWS: usize = 512;

/// A row as measured: the frequencies (Hz) it spans and its columns.
type SrcRow = ((f64, f64), Vec<f32>);

pub struct Waterfall {
    tex: Option<TextureHandle>,
    pub cols: usize,
    head: usize,
    pub filled: usize,
    hist: Vec<f32>,
    /// Each row as it arrived, with the frequencies (Hz) it spans, so a re-map never compounds.
    src: Vec<Option<SrcRow>>,
    /// Engine time each row arrived.
    pub times: Vec<f32>,
    pub generation: u64,
    /// Frequencies (Hz) the columns span, linearly.
    cover: (f64, f64),
    lut: Vec<Color32>,
    key: (Palette, i32, i32),
    full_upload: bool,
}

impl Waterfall {
    pub fn new() -> Waterfall {
        Waterfall {
            tex: None,
            cols: 0,
            head: 0,
            filled: 0,
            hist: Vec::new(),
            src: vec![None; ROWS],
            times: vec![f32::NAN; ROWS],
            generation: u64::MAX,
            cover: (0.0, 0.0),
            lut: Palette::Inferno.lut(),
            key: (Palette::Inferno, i32::MIN, i32::MIN),
            full_upload: true,
        }
    }

    /// Move to a new column layout covering `cover` (Hz). The history is kept: each row is re-mapped
    /// from what it was measured on to the new axis, blank outside the range it covered.
    pub fn relayout(&mut self, cols: usize, generation: u64, cover: (f64, f64)) {
        let cols = cols.max(1);
        let mut hist = vec![f32::NAN; cols * ROWS];
        for i in 0..self.filled {
            let r = (self.head + i) % ROWS;
            if let Some((sc, data)) = &self.src[r] {
                resample(data, *sc, &mut hist[r * cols..(r + 1) * cols], cover);
            }
        }
        self.cols = cols;
        self.cover = cover;
        self.generation = generation;
        self.hist = hist;
        self.full_upload = true;
    }

    pub fn set_levels(&mut self, palette: Palette, lo: f32, hi: f32) {
        let key = (palette, (lo * 10.0) as i32, (hi * 10.0) as i32);
        if key != self.key {
            if key.0 != self.key.0 {
                self.lut = palette.lut();
            }
            self.key = key;
            self.full_upload = true;
        }
    }

    fn color(&self, v: f32) -> Color32 {
        if v.is_nan() {
            return theme::WELL;
        }
        let (lo, hi) = (self.key.1 as f32 / 10.0, self.key.2 as f32 / 10.0);
        let t = ((v - lo) / (hi - lo).max(1.0)).clamp(0.0, 1.0);
        self.lut[(t * 255.0) as usize]
    }

    pub fn push(&mut self, row: &[f32], t: f32) {
        if row.len() != self.cols {
            return;
        }
        self.head = (self.head + ROWS - 1) % ROWS;
        self.hist[self.head * self.cols..(self.head + 1) * self.cols].copy_from_slice(row);
        self.times[self.head] = t;
        self.src[self.head] = Some((self.cover, row.to_vec()));
        self.filled = (self.filled + 1).min(ROWS);
        if !self.full_upload && self.tex.is_some() {
            let px = row.iter().map(|&v| self.color(v)).collect();
            let tex = self.tex.as_mut().unwrap();
            tex.set_partial([0, self.head], ColorImage::new([self.cols, 1], px), TextureOptions::LINEAR);
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        if !self.full_upload && self.tex.as_ref().is_some_and(|t| t.size() == [self.cols, ROWS]) {
            return;
        }
        let px: Vec<Color32> = self.hist.iter().map(|&v| self.color(v)).collect();
        let img = ColorImage::new([self.cols, ROWS], px);
        match &mut self.tex {
            Some(t) if t.size() == [self.cols, ROWS] => t.set(img, TextureOptions::LINEAR),
            _ => self.tex = Some(ctx.load_texture("waterfall", img, TextureOptions::LINEAR)),
        }
        self.full_upload = false;
    }

    /// Time of the row shown `i` rows below the top.
    pub fn row_time(&self, i: usize) -> f32 {
        self.times[(self.head + i) % ROWS]
    }

    /// Draw the newest `rows` rows into `rect`, showing texture columns u0..u1 (0..1 of the cover).
    pub fn paint(&mut self, ctx: &egui::Context, painter: &Painter, rect: Rect, u0: f32, u1: f32, rows: usize) {
        self.upload(ctx);
        let Some(tex) = &self.tex else { return };
        let rows = rows.clamp(8, ROWS);
        let mut mesh = Mesh::with_texture(tex.id());
        let h = ROWS as f32;
        let first = (ROWS - self.head).min(rows);
        let split = rect.top() + rect.height() * first as f32 / rows as f32;
        let v0 = self.head as f32 / h;
        mesh.add_rect_with_uv(
            Rect::from_min_max(rect.min, pos2(rect.right(), split)),
            Rect::from_min_max(pos2(u0, v0), pos2(u1, v0 + first as f32 / h)),
            Color32::WHITE,
        );
        if first < rows {
            let rest = rows - first;
            mesh.add_rect_with_uv(
                Rect::from_min_max(pos2(rect.left(), split), rect.max),
                Rect::from_min_max(pos2(u0, 0.0), pos2(u1, rest as f32 / h)),
                Color32::WHITE,
            );
        }
        painter.add(Shape::mesh(mesh));
    }
}

/// Re-map a row spanning `sc` (Hz) onto `dst` spanning `dc`: the strongest source column under each
/// destination column, NaN where the source has nothing.
fn resample(src: &[f32], sc: (f64, f64), dst: &mut [f32], dc: (f64, f64)) {
    let (n, m) = (src.len(), dst.len());
    if n == m && sc == dc {
        dst.copy_from_slice(src);
        return;
    }
    if n == 0 || sc.1 <= sc.0 {
        return;
    }
    let at = |j: usize| ((dc.0 + (dc.1 - dc.0) * j as f64 / m as f64) - sc.0) / (sc.1 - sc.0) * n as f64;
    for (j, d) in dst.iter_mut().enumerate() {
        let (a, b) = (at(j), at(j + 1));
        if b <= 0.0 || a >= n as f64 {
            continue;
        }
        let a = a.max(0.0).floor() as usize;
        let b = (b.ceil() as usize).clamp(a + 1, n);
        *d = src[a..b].iter().copied().fold(f32::NAN, f32::max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_survives_a_narrow_range_and_back() {
        let mut w = Waterfall::new();
        w.relayout(100, 1, (0.0, 100.0));
        w.push(&(0..100).map(|i| i as f32).collect::<Vec<_>>(), 0.0);
        w.relayout(10, 2, (40.0, 50.0));
        assert_eq!(w.hist[w.head * 10], 40.0);
        w.relayout(100, 3, (0.0, 100.0));
        let row = &w.hist[w.head * 100..(w.head + 1) * 100];
        assert_eq!(row[5], 5.0, "rows outside the narrow range come back");
        assert_eq!(row[99], 99.0);
        w.relayout(50, 4, (50.0, 150.0));
        let row = &w.hist[w.head * 50..(w.head + 1) * 50];
        assert_eq!(row[0], 51.0, "strongest of the source columns under it");
        assert!(row[30].is_nan(), "blank beyond what the row covered");
    }
}
