//! Reading an uploaded spreadsheet into the rows the file viewer lays out.
//!
//! No browser renders a workbook, so unlike every other [`crate::files::
//! ViewerKind`] this one is not a src on an element: the bytes are parsed
//! here and come out as strings the view puts in a `<table>`. Values only —
//! a cell's formula, formatting, charts and images are not carried over.
//!
//! A sheet is read one window at a time, never whole: a page of rows and a
//! page of columns, both moved by links. A book of any size costs the same
//! page, and nothing has to be dragged sideways to reach a column.

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx, open_workbook_auto_from_rs, open_workbook_from_rs};

/// One window of a sheet. Sized to land on a screen rather than to be
/// generous: what does not fit is one link away, and a page that has to be
/// panned is the thing paging exists to avoid.
pub(crate) const ROWS_PER_PAGE: usize = 50;
pub(crate) const COLUMNS_PER_PAGE: usize = 12;

/// One window of one sheet, ready to lay out: every sheet's name for the tab
/// strip, which of them this is, where the window sits, and whether the sheet
/// runs past it.
pub(crate) struct Sheet {
    pub names: Vec<String>,
    pub index: usize,
    pub rows: Vec<Vec<String>>,
    /// Zero-based offsets of the window's own corner, for the row numbers and
    /// column letters the grid draws down its edges.
    pub first_row: usize,
    pub first_column: usize,
    /// The whole sheet's size, when the file says so and the reading agrees.
    /// An xlsx declares its own dimensions and plenty of writers get them
    /// wrong, so a count that cannot be trusted is no count at all rather
    /// than a number the footer would print as fact.
    pub total_rows: Option<usize>,
    pub total_columns: Option<usize>,
    /// Whether there is another window past this one. Observed, not derived
    /// from the count: a cell arriving past the window is proof, and proof is
    /// what a pager needs.
    pub more_rows: bool,
    pub more_columns: bool,
}

impl Sheet {
    /// The window's own last row and column, one-based — what the footer
    /// counts up to.
    pub fn last_row(&self) -> usize {
        self.first_row + self.rows.len()
    }

    pub fn last_column(&self) -> usize {
        self.first_column + self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// The `index`-th sheet of the workbook, windowed to `row_page` and
/// `column_page`. `None` when the bytes are not a workbook any reader here
/// understands — a `.doc` that sniffed as an OLE container, or an `.xlsx`
/// truncated in transit. An out-of-range sheet or page (a hand-edited query
/// string) falls back to the first one rather than failing.
pub(crate) fn read(
    bytes: Vec<u8>,
    index: usize,
    row_page: usize,
    column_page: usize,
) -> Option<Sheet> {
    // xlsx first, because that is the format that holds millions of rows and
    // the only one calamine will hand over a cell at a time. Reading the
    // whole range of a 300,000-row book costs about 380MB and most of a
    // second; stopping at the end of the window costs neither.
    read_streamed(&bytes, index, row_page, column_page)
        .or_else(|| read_whole(bytes, index, row_page, column_page))
}

/// The lazy path: an xlsx read cell by cell in stream order, abandoned the
/// moment the rows past the window start. `None` when the bytes are not an
/// xlsx at all, which is the caller's cue to try the other formats.
fn read_streamed(
    bytes: &[u8],
    index: usize,
    row_page: usize,
    column_page: usize,
) -> Option<Sheet> {
    let mut book: Xlsx<Cursor<Vec<u8>>> = open_workbook_from_rs(Cursor::new(bytes.to_vec())).ok()?;
    let names = book.sheet_names();
    if names.is_empty() {
        return None;
    }
    let index = if index < names.len() { index } else { 0 };
    let mut cells = book.worksheet_cells_reader(&names[index]).ok()?;
    let bounds = cells.dimensions();
    let (sheet_row, sheet_column) = bounds.start;
    let declared_rows = bounds.end.0.saturating_sub(sheet_row) as usize + 1;
    let declared_columns = bounds.end.1.saturating_sub(sheet_column) as usize + 1;
    // The asked-for page is honoured whatever the file declares: an xlsx
    // dimension is a claim, not a fact, and clamping to a wrong one would
    // snap a reader back to the first window every time they stepped on.
    let (first_row, first_column) = (row_page * ROWS_PER_PAGE, column_page * COLUMNS_PER_PAGE);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut more_rows = false;
    let mut more_columns = false;
    let mut seen_rows = 0;
    let mut seen_columns = 0;
    while let Ok(Some(found)) = cells.next_cell() {
        let (row, column) = found.get_position();
        // A cell before the sheet's own first row or column is a dimension
        // that lied; there is nowhere to put it, so it is passed over.
        let (Some(row), Some(column)) = (
            row.checked_sub(sheet_row).map(|row| row as usize),
            column.checked_sub(sheet_column).map(|column| column as usize),
        ) else {
            continue;
        };
        seen_rows = seen_rows.max(row + 1);
        seen_columns = seen_columns.max(column + 1);
        // Cells arrive in row order, so the first one past the window is both
        // the end of the reading and the proof there is another window.
        if row >= first_row + ROWS_PER_PAGE {
            more_rows = true;
            break;
        }
        if column >= first_column + COLUMNS_PER_PAGE {
            more_columns = true;
            continue;
        }
        if row < first_row || column < first_column {
            continue;
        }
        let (row, column) = (row - first_row, column - first_column);
        if rows.len() <= row {
            rows.resize(row + 1, Vec::new());
        }
        let line = &mut rows[row];
        if line.len() <= column {
            line.resize(column + 1, String::new());
        }
        line[column] = cell(&Data::from(found.get_value().clone()));
    }
    // The declared size is believed only where the reading bears it out. A
    // sheet whose cells ran past what it declared has a wrong dimension, and
    // one abandoned mid-read was never counted at all.
    let counted = !more_rows;
    Some(Sheet {
        names,
        index,
        total_rows: (counted && declared_rows >= seen_rows).then_some(declared_rows.max(seen_rows)),
        total_columns: (declared_columns >= seen_columns).then_some(declared_columns),
        more_rows: more_rows || declared_rows > first_row + ROWS_PER_PAGE,
        more_columns: more_columns || declared_columns > first_column + COLUMNS_PER_PAGE,
        rows,
        first_row,
        first_column,
    })
}

/// The whole-workbook path, for the formats with no cell-at-a-time reader:
/// xls, xlsb and ods. All three are bounded by the upload limit and by their
/// own row ceilings, so the range fits in memory in a way an xlsx need not.
fn read_whole(
    bytes: Vec<u8>,
    index: usize,
    row_page: usize,
    column_page: usize,
) -> Option<Sheet> {
    let mut book = open_workbook_auto_from_rs(Cursor::new(bytes)).ok()?;
    let names = book.sheet_names();
    if names.is_empty() {
        return None;
    }
    let index = if index < names.len() { index } else { 0 };
    let range = book.worksheet_range(&names[index]).ok()?;
    let total_rows = range.height();
    let total_columns = range.width();
    let (first_row, first_column) = (row_page * ROWS_PER_PAGE, column_page * COLUMNS_PER_PAGE);
    let rows: Vec<Vec<String>> = range
        .rows()
        .skip(first_row)
        .take(ROWS_PER_PAGE)
        .map(|row| {
            row.iter()
                .skip(first_column)
                .take(COLUMNS_PER_PAGE)
                .map(cell)
                .collect()
        })
        .collect();
    Some(Sheet {
        names,
        index,
        rows,
        first_row,
        first_column,
        // Nothing is declared here: the whole range was read, so its size is
        // measured rather than believed.
        total_rows: Some(total_rows),
        total_columns: Some(total_columns),
        more_rows: total_rows > first_row + ROWS_PER_PAGE,
        more_columns: total_columns > first_column + COLUMNS_PER_PAGE,
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

    /// The window is what keeps a workbook of any size from becoming a page
    /// of that size — and on the streamed path, from being read at all past
    /// the rows it draws.
    #[test]
    fn a_page_holds_one_window_and_the_next_page_holds_the_next() {
        let bytes = include_bytes!("../tests/fixtures/tall.xlsx").to_vec();
        let first = read(bytes.clone(), 0, 0, 0).expect("the workbook opens");
        assert_eq!(first.rows.len(), ROWS_PER_PAGE);
        // This fixture declares no dimension, the way plenty of writers
        // leave one out, so its size is not counted — but the next window is
        // still known to be there, because a cell arrived in it.
        assert_eq!(first.total_rows, None);
        assert!(first.more_rows);
        assert_eq!(first.first_row, 0);
        assert_eq!(first.rows[0][0], "Row 1");
        assert_eq!(first.rows[ROWS_PER_PAGE - 1][0], format!("Row {ROWS_PER_PAGE}"));

        let third = read(bytes.clone(), 0, 2, 0).expect("the workbook opens");
        assert_eq!(third.first_row, 2 * ROWS_PER_PAGE);
        assert_eq!(
            third.rows[0][0],
            format!("Row {}", 2 * ROWS_PER_PAGE + 1)
        );

        // A page nobody links to — typed into the query string — is the
        // empty window it names, with the step back to a real one still
        // there. Nothing pretends it was a different page.
        let past_the_end = read(bytes, 0, 9_000, 0).expect("the workbook opens");
        assert!(past_the_end.rows.is_empty());
        assert!(!past_the_end.more_rows);
    }

    /// A wide sheet pages sideways the same way, and the window's own corner
    /// is what the grid labels its edges from.
    #[test]
    fn columns_page_the_same_way_rows_do() {
        let bytes = include_bytes!("../tests/fixtures/wide.xlsx").to_vec();
        let first = read(bytes.clone(), 0, 0, 0).expect("the workbook opens");
        assert_eq!(first.total_columns, Some(40));
        assert!(first.more_columns);
        assert_eq!(first.rows[0].len(), COLUMNS_PER_PAGE);
        assert_eq!(first.rows[0][0], "Col A");

        let second = read(bytes, 0, 0, 1).expect("the workbook opens");
        assert_eq!(second.first_column, COLUMNS_PER_PAGE);
        assert_eq!(
            second.rows[0][0],
            format!("Col {}", column_name(COLUMNS_PER_PAGE))
        );
    }

    #[test]
    fn bytes_that_are_not_a_workbook_have_no_sheet() {
        assert!(read(b"not a workbook at all".to_vec(), 0, 0, 0).is_none());
    }
}