//! An off-screen grid of styled cells. Frames are drawn into a surface and
//! then diffed against the previous frame, so only changed cells reach the
//! terminal.

use maestro_config::Style;
use maestro_editor::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The grapheme drawn here. Empty for the right half of a wide grapheme.
    pub symbol: String,
    pub style: Style,
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            symbol: " ".into(),
            style,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    pub width: u16,
    pub height: u16,
    cells: Vec<Cell>,
}

impl Surface {
    pub fn new(width: u16, height: u16, style: Style) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::blank(style); width as usize * height as usize],
        }
    }

    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn idx(&self, x: u16, y: u16) -> Option<usize> {
        (x < self.width && y < self.height).then(|| y as usize * self.width as usize + x as usize)
    }

    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        self.idx(x, y).map(|i| &self.cells[i])
    }

    pub fn reset(&mut self, style: Style) {
        for c in &mut self.cells {
            *c = Cell::blank(style);
        }
    }

    pub fn fill(&mut self, area: Rect, style: Style) {
        for y in area.y..area.bottom().min(self.height) {
            for x in area.x..area.right().min(self.width) {
                if let Some(i) = self.idx(x, y) {
                    self.cells[i] = Cell::blank(style);
                }
            }
        }
    }

    /// Layers `style` over the existing style of every cell in `area`.
    pub fn patch(&mut self, area: Rect, style: Style) {
        for y in area.y..area.bottom().min(self.height) {
            for x in area.x..area.right().min(self.width) {
                if let Some(i) = self.idx(x, y) {
                    self.cells[i].style = self.cells[i].style.patch(style);
                }
            }
        }
    }

    /// Puts one grapheme at (x, y). Returns the number of columns used, or 0
    /// if it did not fit before `max_x`.
    pub fn put(&mut self, x: u16, y: u16, grapheme: &str, style: Style, max_x: u16) -> u16 {
        let w = grapheme.width().max(1) as u16;
        if x + w > max_x.min(self.width) || y >= self.height {
            return 0;
        }
        let i = self.idx(x, y).expect("checked bounds");
        self.cells[i] = Cell {
            symbol: grapheme.to_string(),
            style,
        };
        for k in 1..w {
            let j = self.idx(x + k, y).expect("checked bounds");
            self.cells[j] = Cell {
                symbol: String::new(),
                style,
            };
        }
        w
    }

    /// Writes a string, truncating at `max_x`. Returns the next free column.
    pub fn print(&mut self, x: u16, y: u16, s: &str, style: Style, max_x: u16) -> u16 {
        let mut x = x;
        for g in s.graphemes(true) {
            let used = self.put(x, y, g, style, max_x);
            if used == 0 {
                break;
            }
            x += used;
        }
        x
    }

    /// Rows of cells, for diffing.
    pub fn row(&self, y: u16) -> &[Cell] {
        let start = y as usize * self.width as usize;
        &self.cells[start..start + self.width as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_chars_take_two_cells_and_truncate() {
        let mut s = Surface::new(5, 1, Style::default());
        let end = s.print(0, 0, "a漢字b", Style::default(), 5);
        assert_eq!(end, 5);
        assert_eq!(s.cell(1, 0).unwrap().symbol, "漢");
        assert_eq!(s.cell(2, 0).unwrap().symbol, "");
        // "b" did not fit after the second wide char.
        assert_eq!(s.cell(4, 0).unwrap().symbol, "");
        let end = s.print(0, 0, "abcd漢", Style::default(), 5);
        assert_eq!(end, 4);
    }
}
