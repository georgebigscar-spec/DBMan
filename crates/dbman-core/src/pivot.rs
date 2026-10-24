//! Pivot table of a query result: rows grouped by one or two columns, the
//! values of another column spread across, and a number aggregated in each
//! cell, with totals.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::chart::number;
use crate::{Cell, QueryResult};

/// Columns made from distinct values at most; the rest go uncounted except in totals.
pub const MAX_COLUMNS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Agg {
    #[default]
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

impl Agg {
    pub const ALL: [Agg; 6] = [Agg::Count, Agg::CountDistinct, Agg::Sum, Agg::Avg, Agg::Min, Agg::Max];

    pub fn label(self) -> &'static str {
        match self {
            Agg::Count => "Count",
            Agg::CountDistinct => "Count distinct",
            Agg::Sum => "Sum",
            Agg::Avg => "Average",
            Agg::Min => "Minimum",
            Agg::Max => "Maximum",
        }
    }

    /// Whether it needs numbers (the others count any value).
    pub fn numeric(self) -> bool {
        !matches!(self, Agg::Count | Agg::CountDistinct)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PivotSpec {
    /// Columns grouping the rows, outer first (one or two).
    pub rows: Vec<usize>,
    /// The column whose values become columns, if any.
    pub column: Option<usize>,
    /// The column aggregated; `None` counts rows.
    pub value: Option<usize>,
    pub agg: Agg,
}

/// The table to show: a header, rows of text, and whether columns were left out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PivotTable {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// How many of the leading columns are row labels; the rest are numbers.
    pub label_columns: usize,
    pub columns_cut: bool,
    /// The last row holds the totals (its first cell says "Total").
    pub total_row: bool,
}

/// Starts with the first text column as rows and the first numeric one summed.
pub fn default_spec(r: &QueryResult) -> PivotSpec {
    let numeric = crate::chart::numeric_columns(r);
    let row = (0..r.columns.len()).find(|i| !numeric.contains(i)).unwrap_or(0);
    let value = numeric.into_iter().find(|&i| i != row);
    PivotSpec { rows: vec![row], column: None, value, agg: if value.is_some() { Agg::Sum } else { Agg::Count } }
}

#[derive(Debug, Clone, Default)]
struct Acc {
    count: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    distinct: HashSet<String>,
}

impl Acc {
    fn add(&mut self, agg: Agg, cell: &Cell) {
        if cell.is_null() {
            return;
        }
        match agg {
            Agg::Count => self.count += 1,
            Agg::CountDistinct => {
                self.distinct.insert(cell.display().into_owned());
            }
            _ => {
                let Some(v) = number(cell) else { return };
                self.count += 1;
                self.sum += v;
                self.min = Some(self.min.map_or(v, |m| m.min(v)));
                self.max = Some(self.max.map_or(v, |m| m.max(v)));
            }
        }
    }

    fn result(&self, agg: Agg) -> Option<f64> {
        match agg {
            Agg::Count => Some(self.count as f64),
            Agg::CountDistinct => Some(self.distinct.len() as f64),
            Agg::Sum => (self.count > 0).then_some(self.sum),
            Agg::Avg => (self.count > 0).then(|| self.sum / self.count as f64),
            Agg::Min => self.min,
            Agg::Max => self.max,
        }
    }
}

/// A number for a cell: whole numbers plainly, others with up to 4 decimals.
pub fn format(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Orders labels as numbers when both are, else as text; NULL last.
fn compare(a: &str, b: &str) -> Ordering {
    match (a == "NULL", b == "NULL") {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    }
}

pub fn compute(r: &QueryResult, spec: &PivotSpec) -> PivotTable {
    let ncols = r.columns.len();
    let rows: Vec<usize> = spec.rows.iter().copied().filter(|&i| i < ncols).collect();
    let column = spec.column.filter(|&i| i < ncols);
    let value = spec.value.filter(|&i| i < ncols);
    // Counting rows: every row adds one, so count a cell that is never NULL.
    let agg = if value.is_none() { Agg::Count } else { spec.agg };
    let one = Cell::Text("1".into());
    let label = |row: &Vec<Cell>, i: usize| row.get(i).map_or_else(|| "NULL".to_string(), |c| c.display().into_owned());

    let mut col_keys: Vec<String> = Vec::new();
    if let Some(c) = column {
        let mut seen = HashSet::new();
        for row in &r.rows {
            let k = label(row, c);
            if seen.insert(k.clone()) {
                col_keys.push(k);
            }
        }
        col_keys.sort_by(|a, b| compare(a, b));
    }
    let columns_cut = col_keys.len() > MAX_COLUMNS;
    col_keys.truncate(MAX_COLUMNS);
    let col_index: HashMap<&str, usize> = col_keys.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();

    // Per row key: one accumulator per column key, then the row's total.
    let mut groups: BTreeMap<Vec<String>, Vec<Acc>> = BTreeMap::new();
    let mut totals = vec![Acc::default(); col_keys.len() + 1];
    for row in &r.rows {
        let key: Vec<String> = rows.iter().map(|&i| label(row, i)).collect();
        let cell = value.map_or(&one, |v| row.get(v).unwrap_or(&Cell::Null));
        let accs = groups.entry(key).or_insert_with(|| vec![Acc::default(); col_keys.len() + 1]);
        if let Some(c) = column
            && let Some(&i) = col_index.get(label(row, c).as_str())
        {
            accs[i].add(agg, cell);
            totals[i].add(agg, cell);
        }
        accs[col_keys.len()].add(agg, cell);
        totals[col_keys.len()].add(agg, cell);
    }
    let mut keys: Vec<(Vec<String>, Vec<Acc>)> = groups.into_iter().collect();
    keys.sort_by(|(a, _), (b, _)| {
        a.iter().zip(b).map(|(x, y)| compare(x, y)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
    });

    let shown = |acc: &Acc| acc.result(agg).map(format).unwrap_or_default();
    let mut header: Vec<String> = rows.iter().map(|&i| r.columns[i].name.clone()).collect();
    if header.is_empty() {
        header.push(String::new());
    }
    let label_columns = header.len();
    header.extend(col_keys.iter().cloned());
    header.push(if column.is_some() { "Total".into() } else { agg_title(r, value, agg) });
    let mut out: Vec<Vec<String>> = keys
        .iter()
        .map(|(key, accs)| {
            let mut line = if key.is_empty() { vec![String::new()] } else { key.clone() };
            line.extend(accs.iter().map(shown));
            line
        })
        .collect();
    // Without row columns the one group is the total.
    let total_row = rows.is_empty() || out.len() > 1;
    if rows.is_empty() {
        if let Some(line) = out.first_mut() {
            line[0] = "Total".into();
        }
    } else if total_row {
        let mut line = vec![String::new(); label_columns];
        line[0] = "Total".into();
        line.extend(totals.iter().map(shown));
        out.push(line);
    }
    PivotTable { header, rows: out, label_columns, columns_cut, total_row }
}

/// "sum(total)", "count" for the value column's header.
fn agg_title(r: &QueryResult, value: Option<usize>, agg: Agg) -> String {
    match value {
        None => "count".into(),
        Some(v) => format!("{}({})", agg.label().to_lowercase(), r.columns[v].name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    fn result() -> QueryResult {
        let rows = [
            ["Berlin", "2025", "10"],
            ["Berlin", "2026", "5"],
            ["Paris", "2025", "7"],
            ["Berlin", "2025", "1"],
            ["Paris", "2026", "NULL"],
        ];
        QueryResult {
            columns: vec![
                ColumnMeta::new("city", "text"),
                ColumnMeta::new("year", "int4"),
                ColumnMeta::new("n", "int4"),
            ],
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| if *v == "NULL" { Cell::Null } else { Cell::Text(v.to_string()) }).collect())
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn sums_by_row_and_column() {
        let r = result();
        let spec = PivotSpec { rows: vec![0], column: Some(1), value: Some(2), agg: Agg::Sum };
        let t = compute(&r, &spec);
        assert_eq!(t.header, ["city", "2025", "2026", "Total"]);
        assert_eq!(t.rows, [["Berlin", "11", "5", "16"], ["Paris", "7", "", "7"], ["Total", "18", "5", "23"]]);
        assert_eq!(t.label_columns, 1);
    }

    #[test]
    fn counts_and_averages() {
        let r = result();
        let count = compute(&r, &PivotSpec { rows: vec![0], column: None, value: None, agg: Agg::Sum });
        assert_eq!(count.header, ["city", "count"]);
        assert_eq!(count.rows, [["Berlin", "3"], ["Paris", "2"], ["Total", "5"]]);
        let avg = compute(&r, &PivotSpec { rows: vec![1, 0], column: None, value: Some(2), agg: Agg::Avg });
        assert_eq!(avg.header, ["year", "city", "average(n)"]);
        assert_eq!(avg.rows[0], ["2025", "Berlin", "5.5"]);
        assert_eq!(avg.rows.last().unwrap(), &["Total", "", "5.75"]);
        let distinct =
            compute(&r, &PivotSpec { rows: vec![], column: Some(0), value: Some(1), agg: Agg::CountDistinct });
        assert_eq!(distinct.rows, [["Total", "2", "2", "2"]]);
        assert_eq!(default_spec(&r), PivotSpec { rows: vec![0], column: None, value: Some(1), agg: Agg::Sum });
    }

    #[test]
    fn orders_numbers_as_numbers() {
        let mut v = vec!["10", "9", "NULL", "b", "a"];
        v.sort_by(|a, b| compare(a, b));
        assert_eq!(v, ["9", "10", "a", "b", "NULL"]);
        assert_eq!(format(2.0), "2");
        assert_eq!(format(1.0 / 3.0), "0.3333");
    }
}
