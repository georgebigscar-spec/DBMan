//! Profile of a table column: how many values are NULL and distinct, the
//! smallest, largest and average, the most frequent values and, for numbers,
//! a histogram. Each figure is its own query, so a type that cannot be
//! compared or counted distinctly (PostgreSQL `json`, say) loses only that figure.

use crate::sql::Dialect;
use crate::{Cell, Connection, DbKind, DbResult, ObjectPath, StatementOutcome};

/// Most frequent values listed.
pub const TOP: usize = 10;
/// Bars of a histogram.
pub const BUCKETS: usize = 20;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    pub rows: i64,
    pub non_null: i64,
    pub distinct: Option<i64>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub avg: Option<f64>,
    /// Values and how many rows hold them, most frequent first; `None` is NULL.
    pub top: Vec<(Option<String>, i64)>,
    /// Ranges `[lo, hi)` of a numeric column (the last one closed) and their counts.
    pub histogram: Vec<(f64, f64, i64)>,
    /// Figures that could not be read, and why.
    pub errors: Vec<String>,
}

impl Profile {
    pub fn nulls(&self) -> i64 {
        self.rows - self.non_null
    }
}

/// The queries of a profile.
pub struct Queries {
    kind: DbKind,
    table: String,
    col: String,
    /// The `WHERE` filter of the data view, or none.
    filter: String,
}

impl Queries {
    pub fn new(kind: DbKind, path: &ObjectPath, column: &str, filter: &str) -> Self {
        let d = Dialect(kind);
        Self { kind, table: d.qualified(path), col: d.ident(column), filter: filter.trim().to_string() }
    }

    fn source(&self, extra: Option<&str>) -> String {
        let mut conds: Vec<String> = Vec::new();
        if !self.filter.is_empty() {
            conds.push(format!("({})", self.filter));
        }
        conds.extend(extra.map(str::to_string));
        let mut s = format!(" FROM {}", self.table);
        if !conds.is_empty() {
            s.push_str(" WHERE ");
            s.push_str(&conds.join(" AND "));
        }
        s
    }

    pub fn counts(&self) -> String {
        format!("SELECT COUNT(*), COUNT({}){}", self.col, self.source(None))
    }

    pub fn distinct(&self) -> String {
        format!("SELECT COUNT(DISTINCT {}){}", self.col, self.source(None))
    }

    pub fn range(&self, numeric: bool) -> String {
        let c = &self.col;
        let avg = if numeric { format!(", AVG({c} * 1.0)") } else { String::new() };
        format!("SELECT MIN({c}), MAX({c}){avg}{}", self.source(None))
    }

    pub fn top(&self) -> String {
        let c = &self.col;
        let from = self.source(None);
        match self.kind {
            DbKind::MsSql => format!("SELECT TOP {TOP} {c}, COUNT(*){from} GROUP BY {c} ORDER BY 2 DESC"),
            _ => format!("SELECT {c}, COUNT(*){from} GROUP BY {c} ORDER BY 2 DESC LIMIT {TOP}"),
        }
    }

    /// Counts per bucket of width `(max - min) / BUCKETS`, numbered from 0.
    pub fn histogram(&self, min: f64, max: f64) -> String {
        let width = (max - min) / BUCKETS as f64;
        let x = format!("({} - {}) / {}", self.col, num(min), num(width));
        let bucket = match self.kind {
            // SQLite has FLOOR only from 3.35; the values are not below the minimum.
            DbKind::Sqlite => format!("CAST({x} AS INTEGER)"),
            _ => format!("FLOOR({x})"),
        };
        let not_null = format!("{} IS NOT NULL", self.col);
        format!("SELECT b, COUNT(*) FROM (SELECT {bucket} AS b{}) x GROUP BY b", self.source(Some(&not_null)))
    }
}

/// A number as SQL: plain digits, no exponent.
fn num(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('e') { format!("{v:.20}").trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

fn cell_i64(c: &Cell) -> Option<i64> {
    let s = c.display();
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|v| v as i64))
}

fn cell_f64(c: &Cell) -> Option<f64> {
    c.display().trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn text(c: &Cell) -> Option<String> {
    (!c.is_null()).then(|| c.display().into_owned())
}

async fn rows(c: &dyn Connection, db: Option<&str>, sql: &str, limit: usize) -> DbResult<Vec<Vec<Cell>>> {
    match c.execute(db, sql, limit).await? {
        StatementOutcome::Rows(r) => Ok(r.rows),
        _ => Ok(Vec::new()),
    }
}

/// Reads the profile of `column` in the rows of `path` the filter selects.
pub async fn run(
    c: &dyn Connection,
    path: &ObjectPath,
    column: &str,
    numeric: bool,
    filter: &str,
) -> DbResult<Profile> {
    let q = Queries::new(c.kind(), path, column, filter);
    let db = Some(path.database.as_str()).filter(|d| !d.is_empty());
    let mut p = Profile::default();
    // Without counts there is nothing to show: a failure here is the profile's.
    let counts = rows(c, db, &q.counts(), 1).await?;
    let first = counts.first().cloned().unwrap_or_default();
    p.rows = first.first().and_then(cell_i64).unwrap_or(0);
    p.non_null = first.get(1).and_then(cell_i64).unwrap_or(0);
    match rows(c, db, &q.distinct(), 1).await {
        Ok(r) => p.distinct = r.first().and_then(|r| r.first()).and_then(cell_i64),
        Err(e) => p.errors.push(e.message),
    }
    match rows(c, db, &q.range(numeric), 1).await {
        Ok(r) => {
            let r = r.into_iter().next().unwrap_or_default();
            p.min = r.first().and_then(text);
            p.max = r.get(1).and_then(text);
            p.avg = r.get(2).and_then(cell_f64);
        }
        Err(e) => p.errors.push(e.message),
    }
    match rows(c, db, &q.top(), TOP).await {
        Ok(r) => {
            p.top = r.iter().map(|r| (r.first().and_then(text), r.get(1).and_then(cell_i64).unwrap_or(0))).collect()
        }
        Err(e) => p.errors.push(e.message),
    }
    let min = p.min.as_deref().and_then(|s| s.trim().parse::<f64>().ok());
    let max = p.max.as_deref().and_then(|s| s.trim().parse::<f64>().ok());
    // A few distinct values are told by the most frequent ones already.
    if let (true, Some(min), Some(max)) = (numeric && p.distinct.is_none_or(|d| d > TOP as i64), min, max)
        && max > min
    {
        match rows(c, db, &q.histogram(min, max), BUCKETS + 1).await {
            Ok(r) => p.histogram = buckets(min, max, &r),
            Err(e) => p.errors.push(e.message),
        }
    }
    Ok(p)
}

/// The bars of a histogram from `(bucket, count)` rows; the maximum, which
/// falls just past the last bucket, is counted in it.
pub fn buckets(min: f64, max: f64, rows: &[Vec<Cell>]) -> Vec<(f64, f64, i64)> {
    let width = (max - min) / BUCKETS as f64;
    let mut counts = [0i64; BUCKETS];
    for r in rows {
        let (Some(b), Some(n)) = (r.first().and_then(cell_f64), r.get(1).and_then(cell_i64)) else { continue };
        let i = (b.max(0.0) as usize).min(BUCKETS - 1);
        counts[i] += n;
    }
    counts.iter().enumerate().map(|(i, n)| (min + width * i as f64, min + width * (i + 1) as f64, *n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> ObjectPath {
        ObjectPath { database: "shop".into(), schema: "public".into(), name: "orders".into() }
    }

    #[test]
    fn queries() {
        let q = Queries::new(DbKind::Postgres, &path(), "total", "status = 'paid'");
        assert_eq!(q.counts(), r#"SELECT COUNT(*), COUNT("total") FROM "public"."orders" WHERE (status = 'paid')"#);
        assert_eq!(
            q.range(true),
            r#"SELECT MIN("total"), MAX("total"), AVG("total" * 1.0) FROM "public"."orders" WHERE (status = 'paid')"#
        );
        assert_eq!(
            q.top(),
            r#"SELECT "total", COUNT(*) FROM "public"."orders" WHERE (status = 'paid') GROUP BY "total" ORDER BY 2 DESC LIMIT 10"#
        );
        assert_eq!(
            q.histogram(0.0, 100.0),
            r#"SELECT b, COUNT(*) FROM (SELECT FLOOR(("total" - 0) / 5) AS b FROM "public"."orders" WHERE (status = 'paid') AND "total" IS NOT NULL) x GROUP BY b"#
        );
        let ms = Queries::new(DbKind::MsSql, &path(), "total", "");
        assert_eq!(ms.top(), "SELECT TOP 10 [total], COUNT(*) FROM [public].[orders] GROUP BY [total] ORDER BY 2 DESC");
        let lite = Queries::new(DbKind::Sqlite, &path(), "n", "");
        assert!(lite.histogram(1.0, 3.0).contains(r#"CAST(("n" - 1) / 0.1 AS INTEGER)"#));
        assert_eq!(num(1e-7), "0.0000001");
    }

    #[test]
    fn histogram_buckets() {
        let t = |s: &str| Cell::Text(s.into());
        let b = buckets(0.0, 20.0, &[vec![t("0"), t("3")], vec![t("19"), t("1")], vec![t("20"), t("2")]]);
        assert_eq!(b.len(), BUCKETS);
        assert_eq!(b[0], (0.0, 1.0, 3));
        assert_eq!(b[19], (19.0, 20.0, 3));
    }
}
