//! Geometry of the thumbnail grid.
//!
//! Every thumbnail row has the same height, so the vertical position of any photo is known
//! without painting (or loading) the photos above it. The UI paints only the rows that
//! intersect the viewport, which keeps the cost of a frame independent of the number of photos.

use std::collections::HashMap;
use std::ops::Range;

pub const GRID_MIN_CELL: f32 = 80.0;
/// Thumbnails are decoded at 320x240: larger cells would only show upscaled pixels.
pub const GRID_MAX_CELL: f32 = 320.0;
pub const GRID_DEFAULT_CELL: f32 = 160.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridItemKind {
    /// Title of a subfolder; `start..end` are the photos of the group.
    Header,
    /// Row of thumbnails; `start..end` are the photos on this row.
    Row,
}

#[derive(Clone, Copy, Debug)]
pub struct GridItem {
    pub top: f32,
    pub height: f32,
    pub start: usize,
    pub end: usize,
    pub kind: GridItemKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GridMetrics {
    pub width: f32,
    pub gap_x: f32,
    pub gap_y: f32,
    pub name_height: f32,
    pub date_height: f32,
    pub header_height: f32,
    /// Wanted minimum width of a cell: the slider of the toolbar.
    pub min_cell: f32,
}

pub struct GridLayout {
    pub photos_version: u64,
    pub metrics: GridMetrics,
    pub show_filenames: bool,
    pub show_datetime: bool,
    pub items: Vec<GridItem>,
    /// For every photo, the index in `items` of the row that contains it.
    pub photo_item: Vec<u32>,
    pub total_height: f32,
    pub columns: usize,
    pub cell_width: f32,
    pub text_height: f32,
    pub thumb_height: f32,
    pub cell_height: f32,
    /// Cached date range label per group start: (dates version, label).
    pub ranges: HashMap<usize, (u64, Option<String>)>,
}

impl GridLayout {
    pub fn matches(
        &self,
        photos_version: u64,
        metrics: &GridMetrics,
        show_filenames: bool,
        show_datetime: bool,
    ) -> bool {
        self.photos_version == photos_version
            && self.show_filenames == show_filenames
            && self.show_datetime == show_datetime
            && (self.metrics.width - metrics.width).abs() < 0.5
            && self.metrics.gap_x == metrics.gap_x
            && self.metrics.gap_y == metrics.gap_y
            && self.metrics.name_height == metrics.name_height
            && self.metrics.date_height == metrics.date_height
            && self.metrics.header_height == metrics.header_height
            && self.metrics.min_cell == metrics.min_cell
    }

    /// `groups` are the `(start, end)` photo ranges of consecutive photos sharing a subfolder.
    pub fn build(
        groups: &[(usize, usize)],
        photo_count: usize,
        photos_version: u64,
        metrics: GridMetrics,
        show_filenames: bool,
        show_datetime: bool,
    ) -> Self {
        let columns = ((metrics.width + metrics.gap_x) / (metrics.min_cell.clamp(GRID_MIN_CELL, GRID_MAX_CELL) + metrics.gap_x))
            .floor()
            .max(1.0) as usize;
        let cell_width =
            ((metrics.width - metrics.gap_x * (columns - 1) as f32) / columns as f32).max(100.0);
        let mut text_height = 0.0;
        if show_filenames {
            text_height += metrics.name_height;
        }
        if show_datetime {
            text_height += metrics.date_height;
        }
        let thumb_height = cell_width * 0.75;
        let cell_height = text_height + thumb_height;
        let mut items = Vec::with_capacity(groups.len() + photo_count / columns + 1);
        let mut photo_item = vec![0u32; photo_count];
        let mut top = 0.0_f32;
        for &(start, end) in groups {
            items.push(GridItem {
                top,
                height: metrics.header_height,
                start,
                end,
                kind: GridItemKind::Header,
            });
            top += metrics.header_height;
            let mut row = start;
            while row < end {
                let row_end = (row + columns).min(end);
                for slot in &mut photo_item[row..row_end] {
                    *slot = items.len() as u32;
                }
                items.push(GridItem {
                    top,
                    height: cell_height,
                    start: row,
                    end: row_end,
                    kind: GridItemKind::Row,
                });
                top += cell_height + metrics.gap_y;
                row = row_end;
            }
        }
        Self {
            photos_version,
            metrics,
            show_filenames,
            show_datetime,
            items,
            photo_item,
            total_height: top,
            columns,
            cell_width,
            text_height,
            thumb_height,
            cell_height,
            ranges: HashMap::new(),
        }
    }

    /// Items intersecting the vertical span `y_min..=y_max` (content coordinates).
    pub fn visible_items(&self, y_min: f32, y_max: f32) -> Range<usize> {
        let first = self
            .items
            .partition_point(|item| item.top + item.height < y_min);
        let last = self.items.partition_point(|item| item.top <= y_max);
        first..last.max(first)
    }
}

/// Splits consecutive photos sharing a key into `(start, end)` ranges.
pub fn group_ranges<T, K: PartialEq>(items: &[T], key: impl Fn(&T) -> &K) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    for index in 1..=items.len() {
        if index == items.len() || key(&items[index]) != key(&items[start]) {
            if index > start {
                groups.push((start, index));
            }
            start = index;
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(width: f32) -> GridMetrics {
        GridMetrics {
            width,
            gap_x: 8.0,
            gap_y: 8.0,
            name_height: 16.0,
            date_height: 12.0,
            header_height: 40.0,
            min_cell: GRID_DEFAULT_CELL,
        }
    }

    #[test]
    fn groups_split_on_key_change() {
        let keys = ["a", "a", "b", "c", "c", "c"];
        assert_eq!(
            group_ranges(&keys, |key| key),
            vec![(0, 2), (2, 3), (3, 6)]
        );
        assert!(group_ranges::<&str, &str>(&[], |key| key).is_empty());
    }

    #[test]
    fn layout_positions_rows_without_painting() {
        let layout = GridLayout::build(&[(0, 10), (10, 12)], 12, 1, metrics(1000.0), true, true);
        assert_eq!(layout.columns, 6);
        // header + 2 rows, header + 1 row
        assert_eq!(layout.items.len(), 5);
        assert_eq!(layout.photo_item[0], 1);
        assert_eq!(layout.photo_item[5], 1);
        assert_eq!(layout.photo_item[6], 2);
        assert_eq!(layout.photo_item[11], 4);
        let last = layout.items[4];
        assert!((layout.total_height - (last.top + last.height + 8.0)).abs() < 0.01);
        assert!(layout.matches(1, &metrics(1000.2), true, true));
        assert!(!layout.matches(2, &metrics(1000.0), true, true));
        assert!(!layout.matches(1, &metrics(1000.0), false, true));
        let bigger = GridMetrics {
            min_cell: 320.0,
            ..metrics(1000.0)
        };
        assert!(!layout.matches(1, &bigger, true, true));
        assert_eq!(
            GridLayout::build(&[(0, 10)], 10, 1, bigger, true, true).columns,
            3
        );
        assert_eq!(
            GridLayout::build(&[(0, 10)], 10, 1, GridMetrics { min_cell: 9999.0, ..metrics(1000.0) }, true, true).columns,
            3
        );
    }

    #[test]
    fn visible_items_only_returns_intersecting_rows() {
        let groups: Vec<(usize, usize)> = vec![(0, 5000)];
        let layout = GridLayout::build(&groups, 5000, 1, metrics(1000.0), true, true);
        assert_eq!(layout.items.len(), 1 + 5000_usize.div_ceil(6));
        let range = layout.visible_items(10_000.0, 10_800.0);
        assert!(!range.is_empty() && range.len() < 10);
        for item in &layout.items[range.clone()] {
            assert!(item.top <= 10_800.0 && item.top + item.height >= 10_000.0);
        }
        assert!(layout.visible_items(0.0, 1.0).start == 0);
    }
}
