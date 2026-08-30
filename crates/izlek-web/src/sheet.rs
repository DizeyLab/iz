//! Reading an uploaded spreadsheet into the rows the file viewer lays out.
//!
//! No browser renders a workbook, so unlike every other [`crate::files::
//! ViewerKind`] this one is not a src on an element: the bytes are parsed
//! here and come out as strings the view puts in a `<table>`. Values only —
//! a cell's formula, formatting, charts and images are not carried over.

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx, open_workbook_auto_from_rs, open_workbook_from_rs};

/// How much of one sheet is drawn. A workbook is a million rows wide open,
/// and a table that size is a page that never paints; past this the view says
/// so and the download is the whole file.
pub(crate) const MAX_ROWS: usize = 400;
pub(crate) const MAX_COLS: usize = 40;

/// One sheet of a workbook, ready to lay out: every sheet's name for the tab
/// strip, which of them this is, and its cells as display strings.
pub(crate) struct Sheet {
    pub names: Vec<String>,
    pub index: usize,
    pub rows: Vec<Vec<String>>,
    /// True when the sheet ran past [`MAX_ROWS`] or [`MAX_COLS`] and what is
    /// drawn is a corner of it.
    pub clipped: bool,
}

/// The workbook's `index`-th sheet, or `None` when the bytes are not a
/// workbook any reader here understands — a `.doc` that sniffed as an OLE
/// container, or an `.xlsx` truncated in transit. An out-of-range `index`
/// (a hand-edited query string) falls back to the first sheet rather than
/// failing.
pub(crate) fn read(bytes: Vec<u8>, index: usize) -> Option<Sheet> {
    // xlsx first, because that is the format that holds millions of rows and
    // the only one calamine will hand over a cell at a time. Reading the
    // whole range of a 300,000-row book costs about 380MB and most of a
    // second; stopping at the rows that are drawn costs neither.
    read_streamed(&bytes, index).or_else(|| read_whole(bytes, index))
}

/// The lazy path: an xlsx read cell by cell in stream order, abandoned the
/// moment the rows past [`MAX_ROWS`] start. `None` when the bytes are not an
/// xlsx at all, which is the caller's cue to try the other formats.
fn read_streamed(bytes: &[u8], index: usize) -> Option<Sheet> {
    let mut book: Xlsx<Cursor<Vec<u8>>> = open_workbook_from_rs(Cursor::new(bytes.to_vec())).ok()?;
    let names = book.sheet_names();
    if names.is_empty() {
        return None;
    }
    let index = if index < names.len() { index } else { 0 };
    let mut cells = book.worksheet_cells_reader(&names[index]).ok()?;
    let bounds = cells.dimensions();
    let (first_row, first_column) = bounds.start;
    let height = bounds.end.0.saturating_sub(first_row) as usize + 1;
    let width = bounds.end.1.saturating_sub(first_column) as usize + 1;
    let mut clipped = height > MAX_ROWS || width > MAX_COLS;
    let mut rows: Vec<Vec<String>> = Vec::new();
    while let Ok(Some(found)) = cells.next_cell() {
        let (row, column) = found.get_position();
        // A cell before the sheet's own first row or column is a dimension
        // that lied; there is nowhere to put it, so it is passed over.
        let (Some(row), Some(column)) = (
            row.checked_sub(first_row).map(|row| row as usize),
            column.checked_sub(first_column).map(|column| column as usize),
        ) else {
            continue;
        };
        // Cells arrive in row order, so the first one past the last drawn row
        // is the end of the reading — whatever the dimension claimed.
        if row >= MAX_ROWS {
            clipped = true;
            break;
        }
        if column >= MAX_COLS {
            clipped = true;
            continue;
        }
        if rows.len() <= row {
            rows.resize(row + 1, Vec::new());
        }
        let line = &mut rows[row];
        if line.len() <= column {
            line.resize(column + 1, String::new());
        }
        line[column] = cell(&Data::from(found.get_value().clone()));
    }
    Some(Sheet {
        names,
        index,
        rows,
        clipped,
    })
}

/// The whole-workbook path, for the formats with no cell-at-a-time reader:
/// xls, xlsb and ods. All three are bounded by the upload limit and by their
/// own row ceilings, so the range fits in memory in a way an xlsx need not.
fn read_whole(bytes: Vec<u8>, index: usize) -> Option<Sheet> {
    let mut book = open_workbook_auto_from_rs(Cursor::new(bytes)).ok()?;
    let names = book.sheet_names();
    if names.is_empty() {
        return None;
    }
    let index = if index < names.len() { index } else { 0 };
    let range = book.worksheet_range(&names[index]).ok()?;
    let clipped = range.height() > MAX_ROWS || range.width() > MAX_COLS;
    let rows = range
        .rows()
        .take(MAX_ROWS)
        .map(|row| row.iter().take(MAX_COLS).map(cell).collect())
        .collect();
    Some(Sheet {
        names,
        index,
        rows,
        clipped,
    })
}

/// One cell as the table shows it. Excel keeps a date as the number of days
/// since 1899-12-30 and a duration as a fraction of one day, so both would
/// print as a bare float without this; everything else is its own `Display`,
/// and an empty cell is an empty string rather than a placeholder.
fn cell(data: &Data) -> String {
    match data {
        Data::DateTime(stamp) if stamp.is_duration() => {
            let total = (stamp.as_f64() * 86_400.0).round() as i64;
            format!(
                "{}:{:02}:{:02}",
                total / 3600,
                (total / 60) % 60,
                total % 60
            )
        }
        Data::DateTime(stamp) => {
            let (year, month, day, hour, minute, second, _) = stamp.to_ymd_hms_milli();
            if (hour, minute, second) == (0, 0, 0) {
                format!("{year:04}-{month:02}-{day:02}")
            } else {
                format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
            }
        }
        other => other.to_string(),
    }
}

/// The spreadsheet name of a zero-based column: `A`, `Z`, `AA`, `AB`. The
/// grid draws these across the top so a cell can be named out loud without
/// guessing whether the sheet's first row is a header.
pub(crate) fn column_name(index: usize) -> String {
    let mut name = Vec::new();
    let mut left = index;
    loop {
        name.push(b'A' + (left % 26) as u8);
        if left < 26 {
            break;
        }
        left = left / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{ExcelDateTime, ExcelDateTimeType};

    #[test]
    fn empty_cell_is_empty_string() {
        assert_eq!(cell(&Data::Empty), "");
    }

    #[test]
    fn a_whole_number_keeps_no_decimal_point() {
        assert_eq!(cell(&Data::Float(1.0)), "1");
        assert_eq!(cell(&Data::Int(42)), "42");
    }

    #[test]
    fn a_date_reads_as_a_date_not_a_serial_number() {
        let stamp = ExcelDateTime::new(45943.0, ExcelDateTimeType::DateTime, false);
        assert_eq!(cell(&Data::DateTime(stamp)), "2025-10-13");
    }

    #[test]
    fn a_time_of_day_rides_along_with_the_date() {
        let stamp = ExcelDateTime::new(45943.5, ExcelDateTimeType::DateTime, false);
        assert_eq!(cell(&Data::DateTime(stamp)), "2025-10-13 12:00");
    }

    #[test]
    fn a_duration_reads_as_a_clock() {
        let stamp = ExcelDateTime::new(0.5, ExcelDateTimeType::TimeDelta, false);
        assert_eq!(cell(&Data::DateTime(stamp)), "12:00:00");
    }

    #[test]
    fn columns_carry_past_z() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(27), "AB");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }

    /// The clip is what keeps a workbook of any size from becoming a page of
    /// that size — and on the streamed path, from being read at all past the
    /// rows that are drawn.
    #[test]
    fn a_book_past_the_clip_is_read_only_as_far_as_it_is_drawn() {
        let bytes = include_bytes!("../tests/fixtures/tall.xlsx").to_vec();
        let sheet = read(bytes, 0).expect("the workbook opens");
        assert_eq!(sheet.rows.len(), MAX_ROWS);
        assert!(sheet.clipped);
        assert_eq!(sheet.rows[0][0], "Row 1");
        assert_eq!(sheet.rows[MAX_ROWS - 1][0], format!("Row {MAX_ROWS}"));
    }

    #[test]
    fn bytes_that_are_not_a_workbook_have_no_sheet() {
        assert!(read(b"not a workbook at all".to_vec(), 0).is_none());
    }
}