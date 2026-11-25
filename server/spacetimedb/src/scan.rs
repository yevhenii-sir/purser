//! Batches read through an ordered index without reading the rest of the table. SpacetimeDB hands
//! the module every row of a range scan at once: the host serializes the whole range before the
//! module sees its first row, and `.take(n)` stops nothing. A batch read as `filter(from..).take(n)`
//! reads every row from `from` to the end of the table, so a job that walks a table that way reads
//! it again in every step. [`first_rows`] reads growing windows of the column instead.

/// What [`first_rows`] read.
pub struct Rows<T> {
    /// Whole windows of the column in its order: every row whose value is in `from..next`, at
    /// least `n` of them unless the column ends first.
    pub rows: Vec<T>,
    /// Where the next read starts (every row before it was read); `None`: the column was read to
    /// its end.
    pub next: Option<i64>,
}

/// The rows of an ordered column from `from` on, at least `n` of them (when there are), read in
/// windows `read(lo, hi)` (both included) that start `first_window` wide and grow while they come
/// back with too few rows. Once a window passes `end` (the largest value the column normally holds,
/// such as now for a time column), the last read is open: rows after `end` are read too.
///
/// Rows of one value are never split between two reads, as windows are ranges of values.
///
/// A start far before the rows (`i64::MIN`, the epoch) would make the windows grow so wide over
/// the empty part that the first one with rows holds the whole table: once the empty windows are
/// [`SEEK_AFTER`] times `first_window` wide, [`seek`] finds where the rows begin with narrow probes.
pub fn first_rows<T>(from: i64, end: i64, n: usize, first_window: i64, mut read: impl FnMut(i64, i64) -> Vec<T>) -> Rows<T> {
    let first_window = first_window.max(1);
    let mut rows = Vec::new();
    let mut lo = from;
    let mut window = first_window;
    let mut sought = false;
    loop {
        let hi = if lo > end { i64::MAX } else { lo.saturating_add(window - 1).min(end) };
        let got = read(lo, hi);
        let found = got.len();
        rows.extend(got);
        if hi == i64::MAX {
            return Rows { rows, next: None };
        }
        if rows.len() >= n {
            return Rows { rows, next: Some(hi + 1) };
        }
        lo = hi + 1;
        // Few rows: a window twice as wide; none: much wider, or a seek over a long empty stretch.
        window = window.saturating_mul(if found == 0 { 8 } else { 2 });
        if found == 0 && rows.is_empty() && !sought && window / SEEK_AFTER >= first_window && lo <= end {
            sought = true;
            let start = seek(lo, end, (first_window / 60).max(1), &mut read);
            // Everything before the start the probes found (normally nothing: the probes can miss
            // rows between them, never skip them).
            if start > lo {
                rows.extend(read(lo, start - 1));
            }
            lo = start;
            window = first_window;
        }
    }
}

/// How much wider than the first window the empty windows grow before [`first_rows`] seeks.
pub const SEEK_AFTER: i64 = 64;

/// Where the rows begin in `lo..=end`, about: a binary search with probes `probe` wide (each reads
/// only the rows of its probe), for the first probe with rows. `end + 1` when none had any. Rows
/// before the result may exist in the gaps between probes; the caller reads them.
fn seek<T>(lo: i64, end: i64, probe: i64, read: &mut impl FnMut(i64, i64) -> Vec<T>) -> i64 {
    let (mut empty, mut bound) = (lo as i128, end as i128 + 1);
    while bound - empty > probe as i128 {
        let mid = empty + (bound - empty) / 2;
        let m = mid as i64;
        if read(m, m.saturating_add(probe - 1)).is_empty() {
            empty = mid;
        } else {
            bound = mid;
        }
    }
    bound.min(i64::MAX as i128) as i64
}

/// [`first_rows`] of a day column (`i32`).
pub fn first_days<T>(from: i32, end: i32, n: usize, first_window: i32, mut read: impl FnMut(i32, i32) -> Vec<T>) -> Rows<T> {
    let day = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let r = first_rows(from as i64, end as i64, n, first_window as i64, |lo, hi| if lo > i32::MAX as i64 { Vec::new() } else { read(day(lo), day(hi)) });
    Rows { rows: r.rows, next: r.next.filter(|n| *n <= i32::MAX as i64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(values: &[i64]) -> impl Fn(i64, i64) -> Vec<i64> + '_ {
        move |lo, hi| values.iter().copied().filter(|v| (lo..=hi).contains(v)).collect()
    }

    #[test]
    fn reads_growing_windows_until_enough_rows() {
        let values: Vec<i64> = (0..1000).map(|i| i * 10).collect();
        let mut reads = Vec::new();
        let r = first_rows(0, 20_000, 50, 100, |lo, hi| {
            reads.push((lo, hi));
            column(&values)(lo, hi)
        });
        assert!(r.rows.len() >= 50 && r.rows.len() < 120, "{}", r.rows.len());
        assert_eq!(r.rows, (0..r.rows.len() as i64).map(|i| i * 10).collect::<Vec<_>>(), "in order, nothing skipped");
        let next = r.next.unwrap();
        assert_eq!(next, reads.last().unwrap().1 + 1);
        // The next read starts where this one stopped and reads the rest to the end.
        let rest = first_rows(next, 20_000, 10_000, 100, column(&values));
        assert_eq!(r.rows.len() + rest.rows.len(), 1000);
        assert_eq!(rest.next, None);
    }

    #[test]
    fn rows_after_the_usual_end_and_before_a_far_start_are_found() {
        let values = [-5, 3, 1_000_000_000, i64::MAX - 1];
        let r = first_rows(i64::MIN, 1_000, 100, 60, column(&values));
        assert_eq!((r.rows, r.next), (values.to_vec(), None));
    }

    #[test]
    fn a_start_far_before_the_rows_seeks_them_instead_of_reading_the_table() {
        // 100,000 rows, one a second from a recent day; a few stray old ones far before them.
        let base = 1_790_000_000_000_000i64;
        let mut values: Vec<i64> = (0..100_000).map(|i| base + i * 1_000_000).collect();
        values.splice(0..0, [5, base - 400 * 86_400_000_000]);
        let read_rows = std::cell::Cell::new(0usize);
        let read = |lo: i64, hi: i64| {
            let lo_i = values.partition_point(|v| *v < lo);
            let hi_i = values.partition_point(|v| *v <= hi);
            read_rows.set(read_rows.get() + (hi_i - lo_i.min(hi_i)));
            values[lo_i.min(hi_i)..hi_i].to_vec()
        };
        let r = first_rows(i64::MIN, base + 200_000_000_000, 2_000, 3_600_000_000, read);
        assert_eq!(&r.rows[..2], &[5, base - 400 * 86_400_000_000], "the stray rows first");
        assert_eq!(r.rows[2], base);
        assert!(r.rows.len() >= 2_000 && r.rows.len() < 20_000, "{}", r.rows.len());
        assert!(read_rows.get() < 30_000, "read {} rows of 100,002", read_rows.get());
        // The rest follows on.
        let rest = first_rows(r.next.unwrap(), base + 200_000_000_000, 1_000_000, 3_600_000_000, read);
        assert_eq!(r.rows.len() + rest.rows.len(), values.len());
    }

    #[test]
    fn a_dense_value_is_read_whole() {
        let values = [7, 7, 7, 7, 7, 30];
        let r = first_rows(0, 100, 2, 1, column(&values));
        assert_eq!(r.rows, vec![7; 5]);
        assert_eq!(r.next, Some(9));
    }

    #[test]
    fn days() {
        let values = [19_000i64, 19_001, 20_000, 20_001];
        let read = |lo: i32, hi: i32| column(&values)(lo as i64, hi as i64);
        let first = first_days(19_000, 20_500, 2, 1, read);
        assert_eq!(first.rows, vec![19_000, 19_001]);
        let rest = first_days(first.next.unwrap() as i32, 20_500, 10, 1, read);
        assert_eq!((rest.rows, rest.next), (vec![20_000, 20_001], None));
        assert_eq!(first_days(i32::MIN, 20_500, 10, 1, read).rows.len(), 4, "from the first day there is");
    }
}
