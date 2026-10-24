//! Charts of a query result: which columns can be plotted, and the series and
//! axis ticks a bar, line or pie chart draws.

use crate::{Cell, QueryResult};

/// Points drawn at most; bar and line charts of longer results show the first ones.
pub const MAX_POINTS: usize = 1000;
/// Slices of a pie; the smaller ones are summed into "Other".
pub const MAX_SLICES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
    Pie,
}

impl ChartKind {
    pub const ALL: [ChartKind; 3] = [ChartKind::Bar, ChartKind::Line, ChartKind::Pie];

    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "Bars",
            ChartKind::Line => "Lines",
            ChartKind::Pie => "Pie",
        }
    }
}

/// What to draw: the categories on the X axis and the columns plotted against them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub x: usize,
    pub ys: Vec<usize>,
}

/// One plotted column.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    /// One value per label; `None` for NULL or text that is not a number.
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChartData {
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    /// Rows left out past [`MAX_POINTS`].
    pub skipped: usize,
}

impl ChartData {
    /// The smallest and largest value of all series, with 0 included so bars start at the axis.
    pub fn range(&self) -> (f64, f64) {
        let values = self.series.iter().flat_map(|s| s.values.iter().flatten().copied());
        values.fold((0.0, 0.0), |(lo, hi), v| (lo.min(v), hi.max(v)))
    }
}

/// A cell as a number, if it is one.
pub fn number(c: &Cell) -> Option<f64> {
    match c {
        Cell::Text(t) => t.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// Columns whose values are numbers: by their type, or every value that is not NULL parses as one.
pub fn numeric_columns(r: &QueryResult) -> Vec<usize> {
    (0..r.columns.len())
        .filter(|&i| {
            let mut values = r.rows.iter().map(|row| row.get(i).unwrap_or(&Cell::Null)).filter(|c| !c.is_null());
            if r.columns[i].numeric {
                return true;
            }
            let mut any = false;
            values.all(|c| {
                any = true;
                number(c).is_some()
            }) && any
        })
        .collect()
}

/// The chart a result opens with: the first column that is not a number on the X axis
/// (or the first column), and the other numeric columns as series. `None` without
/// a numeric column to plot.
pub fn default_spec(r: &QueryResult) -> Option<ChartSpec> {
    let numeric = numeric_columns(r);
    let x = (0..r.columns.len()).find(|i| !numeric.contains(i)).unwrap_or(0);
    let ys: Vec<usize> = numeric.into_iter().filter(|&i| i != x).take(4).collect();
    if ys.is_empty() {
        return None;
    }
    Some(ChartSpec { kind: ChartKind::Bar, x, ys })
}

/// The labels and series of `spec` over the rows of `r`.
pub fn data(r: &QueryResult, spec: &ChartSpec) -> ChartData {
    let cell = |row: &Vec<Cell>, i: usize| row.get(i).cloned().unwrap_or(Cell::Null);
    let rows = &r.rows[..r.rows.len().min(MAX_POINTS)];
    let labels = rows.iter().map(|row| cell(row, spec.x).display().into_owned()).collect();
    let series = spec
        .ys
        .iter()
        .filter(|&&y| y < r.columns.len())
        .map(|&y| Series {
            name: r.columns[y].name.clone(),
            values: rows.iter().map(|row| number(&cell(row, y))).collect(),
        })
        .collect();
    let mut data = ChartData { labels, series, skipped: r.rows.len() - rows.len() };
    if spec.kind == ChartKind::Pie {
        data = pie(&r.rows, spec, r.columns.get(spec.ys.first().copied().unwrap_or(0)).map_or("", |c| &c.name));
    }
    data
}

/// The slices of a pie: the first series' positive values summed by label, largest first,
/// with the ones past [`MAX_SLICES`] summed into "Other".
fn pie(rows: &[Vec<Cell>], spec: &ChartSpec, name: &str) -> ChartData {
    let Some(&y) = spec.ys.first() else { return ChartData::default() };
    let mut sums: Vec<(String, f64)> = Vec::new();
    for row in rows {
        let Some(v) = row.get(y).and_then(number).filter(|v| *v > 0.0) else { continue };
        let label = row.get(spec.x).map_or_else(String::new, |c| c.display().into_owned());
        match sums.iter_mut().find(|(l, _)| *l == label) {
            Some((_, s)) => *s += v,
            None => sums.push((label, v)),
        }
    }
    sums.sort_by(|a, b| b.1.total_cmp(&a.1));
    if sums.len() > MAX_SLICES {
        let other: f64 = sums[MAX_SLICES - 1..].iter().map(|(_, v)| v).sum();
        sums.truncate(MAX_SLICES - 1);
        sums.push(("Other".into(), other));
    }
    let (labels, values) = sums.into_iter().map(|(l, v)| (l, Some(v))).unzip();
    ChartData { labels, series: vec![Series { name: name.to_string(), values }], skipped: 0 }
}

/// About `count` round values from `lo` to `hi` for an axis: steps of 1, 2 or 5 times a power of ten.
pub fn ticks(lo: f64, hi: f64, count: usize) -> Vec<f64> {
    let (lo, hi) = match () {
        _ if hi > lo => (lo, hi),
        // All zeros: an axis from 0 to 1 rather than one going below zero.
        _ if lo == 0.0 => (0.0, 1.0),
        _ => (lo - 1.0, lo + 1.0),
    };
    let raw = (hi - lo) / count.max(1) as f64;
    let power = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * power).find(|s| *s >= raw).unwrap_or(10.0 * power);
    let first = (lo / step).floor() as i64;
    let last = (hi / step).ceil() as i64;
    (first..=last).map(|i| i as f64 * step).collect()
}

/// A tick or value for an axis: "1.5k", "2M", "0.25".
pub fn short_number(v: f64) -> String {
    let a = v.abs();
    let (n, unit) = if a >= 1e12 {
        (v / 1e12, "T")
    } else if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e4 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    let s = format!("{n:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{}{unit}", if s == "-0" { "0" } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    fn result(cols: &[(&str, &str)], rows: &[&[&str]]) -> QueryResult {
        QueryResult {
            columns: cols.iter().map(|(n, t)| ColumnMeta::new(*n, *t)).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| if *v == "NULL" { Cell::Null } else { Cell::Text(v.to_string()) }).collect())
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn picks_columns() {
        // SQLite reports no types: numbers are told by their values.
        let r = result(
            &[("city", ""), ("orders", ""), ("total", "numeric"), ("note", "")],
            &[&["Berlin", "3", "120.5", "x"], &["Paris", "NULL", "80", "1"]],
        );
        assert_eq!(numeric_columns(&r), [1, 2]);
        assert_eq!(default_spec(&r), Some(ChartSpec { kind: ChartKind::Bar, x: 0, ys: vec![1, 2] }));
        let d = data(&r, &default_spec(&r).unwrap());
        assert_eq!(d.labels, ["Berlin", "Paris"]);
        assert_eq!(d.series[0].values, [Some(3.0), None]);
        assert_eq!(d.series[1].name, "total");
        assert_eq!(d.range(), (0.0, 120.5));

        let text = result(&[("a", "text")], &[&["x"]]);
        assert_eq!(default_spec(&text), None);
        // Only numbers: the first one is the X axis.
        let nums = result(&[("year", "int4"), ("n", "int8")], &[&["2024", "5"]]);
        assert_eq!(default_spec(&nums).unwrap().ys, [1]);
    }

    #[test]
    fn pie_slices() {
        let mut rows: Vec<Vec<String>> = (0..15).map(|i| vec![format!("c{i}"), format!("{}", 100 - i)]).collect();
        rows.push(vec!["c0".into(), "50".into()]);
        rows.push(vec!["neg".into(), "-5".into()]);
        let refs: Vec<Vec<&str>> = rows.iter().map(|r| r.iter().map(String::as_str).collect()).collect();
        let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
        let r = result(&[("c", "text"), ("v", "int4")], &slices);
        let d = data(&r, &ChartSpec { kind: ChartKind::Pie, x: 0, ys: vec![1] });
        assert_eq!(d.labels.len(), MAX_SLICES);
        assert_eq!(d.labels[0], "c0");
        assert_eq!(d.series[0].values[0], Some(150.0));
        assert_eq!(d.labels.last().unwrap(), "Other");
        // c11..c14: 89 + 88 + 87 + 86.
        assert_eq!(d.series[0].values.last().unwrap(), &Some(350.0));
    }

    #[test]
    fn axis_ticks() {
        assert_eq!(ticks(0.0, 120.5, 5), [0.0, 50.0, 100.0, 150.0]);
        assert_eq!(ticks(0.0, 1.0, 4), [0.0, 0.5, 1.0]);
        assert_eq!(ticks(-30.0, 0.0, 3), [-30.0, -20.0, -10.0, 0.0]);
        assert_eq!(ticks(0.0, 0.0, 4), [0.0, 0.5, 1.0]);
        assert_eq!(ticks(5.0, 5.0, 4), [4.0, 4.5, 5.0, 5.5, 6.0]);
        assert_eq!(short_number(1500.0), "1500");
        assert_eq!(short_number(25_000.0), "25k");
        assert_eq!(short_number(2_500_000.0), "2.5M");
        assert_eq!(short_number(0.25), "0.25");
    }
}
