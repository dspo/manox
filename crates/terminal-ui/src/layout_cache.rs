//! Per-line shaped-run cache.
//!
//! Shaping every text run on every repaint is the terminal's largest render
//! cost, and most frames repaint lines whose content did not change (a
//! prompt echo, a scrolling log). Runs are cached per alacritty grid line —
//! a coordinate stable across scrolls — keyed by a content fingerprint;
//! paint positions are recomputed per frame, so a scroll reuses the shaped
//! glyphs verbatim. A `clear` (theme / font change) invalidates everything,
//! and a per-frame sweep bounds the map to the visible window.

use std::collections::HashMap;

use terminal::{Cell, Color};

/// One cached line: the shaped runs (start column + value) and the content
/// fingerprint they were built from.
struct LineEntry<V> {
    fingerprint: u64,
    runs: Vec<(i32, V)>,
    /// Touched by `get`/`insert` since the last sweep; dropped otherwise.
    seen: bool,
}

/// Grid-line → shaped runs map. Generic over the shaped value so the
/// fingerprint/sweep logic is testable without a text system.
pub struct LineShapeCache<V> {
    lines: HashMap<i32, LineEntry<V>>,
}

impl<V: Clone> Default for LineShapeCache<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Clone> LineShapeCache<V> {
    pub fn new() -> Self {
        Self {
            lines: HashMap::new(),
        }
    }

    /// Drop every entry (theme / font change invalidates shaped glyphs).
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// The cached runs for `grid_line`, or `None` when the line's content
    /// changed since the entry was stored.
    pub fn get(&mut self, grid_line: i32, fingerprint: u64) -> Option<Vec<(i32, V)>> {
        let entry = self.lines.get_mut(&grid_line)?;
        if entry.fingerprint != fingerprint {
            return None;
        }
        entry.seen = true;
        Some(entry.runs.clone())
    }

    /// Store freshly-shaped runs for `grid_line`, replacing any stale entry.
    pub fn insert(&mut self, grid_line: i32, fingerprint: u64, runs: Vec<(i32, V)>) {
        self.lines.insert(
            grid_line,
            LineEntry {
                fingerprint,
                runs,
                seen: true,
            },
        );
    }

    /// Drop entries nothing touched this frame and re-arm the flags. Keeps
    /// the map bounded by the visible window as the grid scrolls.
    pub fn sweep(&mut self) {
        self.lines.retain(|_, e| e.seen);
        for e in self.lines.values_mut() {
            e.seen = false;
        }
    }
}

/// FNV-1a fingerprint over one line's cells, covering every field a shaped
/// run depends on: char, fg, bg, flags. Content-identical lines hash equal;
/// a single-cell change re-shapes just that line.
pub fn line_fingerprint<'a>(cells: impl Iterator<Item = &'a Cell>) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut mix = |byte: u8| {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    let mut buf = [0u8; 4];
    for cell in cells {
        for &b in cell.c.encode_utf8(&mut buf).as_bytes() {
            mix(b);
        }
        hash_color(&mut mix, &cell.fg);
        hash_color(&mut mix, &cell.bg);
        for &b in &cell.flags.bits().to_le_bytes() {
            mix(b);
        }
    }
    hash
}

fn hash_color(mix: &mut impl FnMut(u8), color: &Color) {
    match color {
        Color::Named(n) => {
            mix(0);
            mix(*n as u8);
        }
        Color::Indexed(i) => {
            mix(1);
            mix(*i);
        }
        Color::Spec(rgb) => {
            mix(2);
            mix(rgb.r);
            mix(rgb.g);
            mix(rgb.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::Flags;

    fn cell(c: char, fg: Color, bg: Color, flags: Flags) -> Cell {
        Cell {
            c,
            fg,
            bg,
            flags,
            extra: None,
        }
    }

    #[test]
    fn identical_lines_fingerprint_equal() {
        let a = [
            cell(
                'h',
                Color::Named(terminal::NamedColor::Foreground),
                Color::Named(terminal::NamedColor::Background),
                Flags::empty(),
            ),
            cell(
                'i',
                Color::Named(terminal::NamedColor::Foreground),
                Color::Named(terminal::NamedColor::Background),
                Flags::empty(),
            ),
        ];
        let b = a.clone();
        assert_eq!(line_fingerprint(a.iter()), line_fingerprint(b.iter()));
    }

    #[test]
    fn content_change_changes_fingerprint() {
        let base = [
            cell(
                'a',
                Color::Named(terminal::NamedColor::Foreground),
                Color::Named(terminal::NamedColor::Background),
                Flags::empty(),
            ),
            cell(
                'b',
                Color::Named(terminal::NamedColor::Foreground),
                Color::Named(terminal::NamedColor::Background),
                Flags::empty(),
            ),
        ];
        let fp = line_fingerprint(base.iter());

        let mut by_char = base.clone();
        by_char[1].c = 'c';
        assert_ne!(fp, line_fingerprint(by_char.iter()));

        let mut by_fg = base.clone();
        by_fg[0].fg = Color::Named(terminal::NamedColor::Red);
        assert_ne!(fp, line_fingerprint(by_fg.iter()));

        let mut by_bg = base.clone();
        by_bg[0].bg = Color::Spec(terminal::Rgb { r: 1, g: 2, b: 3 });
        assert_ne!(fp, line_fingerprint(by_bg.iter()));

        let mut by_flags = base.clone();
        by_flags[0].flags = Flags::BOLD;
        assert_ne!(fp, line_fingerprint(by_flags.iter()));

        // Indexed colors with different indices differ.
        let mut by_index = base.clone();
        by_index[0].fg = Color::Indexed(42);
        assert_ne!(fp, line_fingerprint(by_index.iter()));
    }

    #[test]
    fn cache_hit_requires_matching_fingerprint() {
        let mut cache: LineShapeCache<String> = LineShapeCache::new();
        cache.insert(3, 100, vec![(0, "run".to_string())]);
        assert!(cache.get(3, 100).is_some());
        assert!(cache.get(3, 101).is_none());
        assert!(cache.get(4, 100).is_none());
    }

    #[test]
    fn sweep_drops_untouched_entries() {
        let mut cache: LineShapeCache<String> = LineShapeCache::new();
        cache.insert(1, 10, vec![(0, "a".to_string())]);
        cache.insert(2, 20, vec![(0, "b".to_string())]);
        // Frame 1 sweep: both freshly inserted entries survive, flags re-arm.
        cache.sweep();
        // Frame 2 touches only line 1.
        assert!(cache.get(1, 10).is_some());
        cache.sweep();
        assert!(cache.get(1, 10).is_some());
        assert!(!cache.lines.contains_key(&2));
    }

    #[test]
    fn clear_invalidates_everything() {
        let mut cache: LineShapeCache<String> = LineShapeCache::new();
        cache.insert(1, 10, vec![(0, "a".to_string())]);
        cache.clear();
        assert!(cache.get(1, 10).is_none());
    }
}
