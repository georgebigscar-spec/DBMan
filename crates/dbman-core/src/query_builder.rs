//! A query put together from tables and their columns rather than typed:
//! tables joined on their foreign keys (or on columns picked by hand), the
//! columns to show with aggregates, filters, sorting and a row limit, turned
//! into the engine's SQL.

use crate::pivot::Agg;
use crate::result::is_numeric_type;
use crate::sql::Dialect;
use crate::{DbKind, ObjectPath, TableDetails};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinKind {
    #[default]
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    pub const ALL: [JoinKind; 4] = [JoinKind::Inner, JoinKind::Left, JoinKind::Right, JoinKind::Full];

    pub fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            JoinKind::Inner => "Inner join",
            JoinKind::Left => "Left join",
            JoinKind::Right => "Right join",
            JoinKind::Full => "Full join",
        }
    }

    /// The same join seen from the other table.
    fn flipped(self) -> Self {
        match self {
            JoinKind::Left => JoinKind::Right,
            JoinKind::Right => JoinKind::Left,
            k => k,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QTable {
    pub path: ObjectPath,
    /// The name the query uses for it: the table's name, or `name_2` for a
    /// table added again.
    pub alias: String,
    pub details: TableDetails,
}

/// `left KIND JOIN right ON left.a = right.b AND …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub left: usize,
    pub right: usize,
    /// Column of `left`, column of `right`.
    pub on: Vec<(String, String)>,
    pub kind: JoinKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    None,
    Asc,
    Desc,
}

/// A column shown in the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub table: usize,
    pub column: String,
    /// The name in the result (`AS …`), when not empty.
    pub alias: String,
    pub agg: Option<Agg>,
    pub sort: Sort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Op {
    #[default]
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    In,
    IsNull,
    IsNotNull,
}

impl Op {
    pub const ALL: [Op; 11] =
        [Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Like, Op::NotLike, Op::In, Op::IsNull, Op::IsNotNull];

    pub fn label(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "<>",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Like => "LIKE",
            Op::NotLike => "NOT LIKE",
            Op::In => "IN",
            Op::IsNull => "IS NULL",
            Op::IsNotNull => "IS NOT NULL",
        }
    }

    /// Whether it compares with a value.
    pub fn takes_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::IsNotNull)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub table: usize,
    pub column: String,
    pub op: Op,
    /// As typed; `IN` takes values separated by commas.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub tables: Vec<QTable>,
    pub joins: Vec<Join>,
    pub outputs: Vec<Output>,
    pub filters: Vec<Filter>,
    pub distinct: bool,
    pub limit: Option<u64>,
}

/// Whether a foreign key of a table in `of` points to `to`.
fn references(of: &ObjectPath, ref_schema: &str, ref_table: &str, to: &ObjectPath) -> bool {
    // Engines without schemas report the database (or nothing) as the schema.
    let schema = if ref_schema.is_empty() || ref_schema == of.database { of.schema.as_str() } else { ref_schema };
    ref_table == to.name && schema == to.schema
}

impl Query {
    /// Adds a table, joined on a foreign key to each table already there that
    /// it references or that references it. Returns its index.
    pub fn add_table(&mut self, path: ObjectPath, details: TableDetails) -> usize {
        let taken = |a: &str, tables: &[QTable]| tables.iter().any(|t| t.alias == a);
        let mut alias = path.name.clone();
        let mut n = 2;
        while taken(&alias, &self.tables) {
            alias = format!("{}_{n}", path.name);
            n += 1;
        }
        let new = self.tables.len();
        self.tables.push(QTable { path, alias, details });
        for other in 0..new {
            if let Some(j) = self.fk_join(new, other).or_else(|| self.fk_join(other, new)) {
                self.joins.push(j);
            }
        }
        new
    }

    /// The first foreign key of `child` that points to `parent`, as a join.
    fn fk_join(&self, child: usize, parent: usize) -> Option<Join> {
        let (c, p) = (&self.tables[child], &self.tables[parent]);
        let fk = c.details.foreign_keys.iter().find(|f| references(&c.path, &f.ref_schema, &f.ref_table, &p.path))?;
        let on = fk.columns.iter().cloned().zip(fk.ref_columns.iter().cloned()).collect();
        Some(Join { left: child, right: parent, on, kind: JoinKind::Inner })
    }

    /// Removes a table with its joins, outputs and filters.
    pub fn remove_table(&mut self, i: usize) {
        if i >= self.tables.len() {
            return;
        }
        self.tables.remove(i);
        let shift = |t: &mut usize| {
            if *t > i {
                *t -= 1;
            }
        };
        self.joins.retain(|j| j.left != i && j.right != i);
        for j in &mut self.joins {
            shift(&mut j.left);
            shift(&mut j.right);
        }
        self.outputs.retain(|o| o.table != i);
        self.outputs.iter_mut().for_each(|o| shift(&mut o.table));
        self.filters.retain(|f| f.table != i);
        self.filters.iter_mut().for_each(|f| shift(&mut f.table));
    }

    /// Joins two columns by hand: adds the pair to a join of the two tables,
    /// or makes a new one.
    pub fn link(&mut self, a: usize, a_col: &str, b: usize, b_col: &str) {
        if a == b {
            return;
        }
        if let Some(j) = self.joins.iter_mut().find(|j| (j.left, j.right) == (a, b)) {
            j.on.push((a_col.into(), b_col.into()));
        } else if let Some(j) = self.joins.iter_mut().find(|j| (j.left, j.right) == (b, a)) {
            j.on.push((b_col.into(), a_col.into()));
        } else {
            self.joins.push(Join { left: a, right: b, on: vec![(a_col.into(), b_col.into())], kind: JoinKind::Inner });
        }
    }

    /// Whether `column` of `table` is in the result.
    pub fn is_output(&self, table: usize, column: &str) -> bool {
        self.outputs.iter().any(|o| o.table == table && o.column == column)
    }

    /// Adds the column to the result, or takes it out.
    pub fn toggle_output(&mut self, table: usize, column: &str) {
        if let Some(i) = self.outputs.iter().position(|o| o.table == table && o.column == column) {
            self.outputs.remove(i);
        } else {
            self.outputs.push(Output {
                table,
                column: column.into(),
                alias: String::new(),
                agg: None,
                sort: Sort::None,
            });
        }
    }

    fn numeric(&self, table: usize, column: &str) -> bool {
        self.tables[table].details.columns.iter().any(|c| c.name == column && is_numeric_type(&c.data_type))
    }

    /// The query's SQL; empty without tables.
    pub fn sql(&self, kind: DbKind) -> String {
        if self.tables.is_empty() {
            return String::new();
        }
        let d = Dialect(kind);
        let many = self.tables.len() > 1;
        let col = |t: usize, c: &str| {
            if many { format!("{}.{}", d.ident(&self.tables[t].alias), d.ident(c)) } else { d.ident(c) }
        };
        let from = |t: usize| {
            let q = &self.tables[t];
            let name = d.qualified(&q.path);
            if q.alias == q.path.name { name } else { format!("{name} AS {}", d.ident(&q.alias)) }
        };

        let mut sql = String::from("SELECT");
        if self.distinct {
            sql.push_str(" DISTINCT");
        }
        if kind == DbKind::MsSql
            && let Some(n) = self.limit
        {
            sql.push_str(&format!(" TOP {n}"));
        }
        let expr = |o: &Output| {
            let c = col(o.table, &o.column);
            match o.agg {
                None => c,
                Some(Agg::Count) => format!("COUNT({c})"),
                Some(Agg::CountDistinct) => format!("COUNT(DISTINCT {c})"),
                Some(Agg::Sum) => format!("SUM({c})"),
                Some(Agg::Avg) => format!("AVG({c})"),
                Some(Agg::Min) => format!("MIN({c})"),
                Some(Agg::Max) => format!("MAX({c})"),
            }
        };
        if self.outputs.is_empty() {
            sql.push_str("\n  *");
        } else {
            let list: Vec<String> = self
                .outputs
                .iter()
                .map(|o| {
                    let e = expr(o);
                    if o.alias.trim().is_empty() { e } else { format!("{e} AS {}", d.ident(o.alias.trim())) }
                })
                .collect();
            sql.push_str(&format!("\n  {}", list.join(",\n  ")));
        }

        // Each table joins the tables before it; one with no join to them is a cross join.
        sql.push_str(&format!("\nFROM {}", from(0)));
        let mut placed = vec![0];
        while placed.len() < self.tables.len() {
            let joined = (0..self.tables.len()).find(|t| {
                !placed.contains(t)
                    && self.joins.iter().any(|j| {
                        (j.left == *t && placed.contains(&j.right)) || (j.right == *t && placed.contains(&j.left))
                    })
            });
            let Some(t) = joined else {
                let t = (0..self.tables.len()).find(|t| !placed.contains(t)).unwrap_or_default();
                sql.push_str(&format!("\n  CROSS JOIN {}", from(t)));
                placed.push(t);
                continue;
            };
            let mut kind_of = None;
            let mut on = Vec::new();
            for j in &self.joins {
                let (other, k, pairs): (usize, JoinKind, Vec<(&str, &str)>) = if j.right == t {
                    (j.left, j.kind, j.on.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect())
                } else if j.left == t {
                    (j.right, j.kind.flipped(), j.on.iter().map(|(a, b)| (b.as_str(), a.as_str())).collect())
                } else {
                    continue;
                };
                if !placed.contains(&other) {
                    continue;
                }
                kind_of.get_or_insert(k);
                on.extend(pairs.into_iter().map(|(o, n)| format!("{} = {}", col(t, n), col(other, o))));
            }
            let on = if on.is_empty() { "1 = 1".to_string() } else { on.join(" AND ") };
            sql.push_str(&format!("\n  {} {} ON {on}", kind_of.unwrap_or_default().sql(), from(t)));
            placed.push(t);
        }

        let conditions: Vec<String> =
            self.filters.iter().map(|f| self.condition(d, f, col(f.table, &f.column))).collect();
        if !conditions.is_empty() {
            sql.push_str(&format!("\nWHERE {}", conditions.join("\n  AND ")));
        }
        if self.outputs.iter().any(|o| o.agg.is_some()) {
            let groups: Vec<String> =
                self.outputs.iter().filter(|o| o.agg.is_none()).map(|o| col(o.table, &o.column)).collect();
            if !groups.is_empty() {
                sql.push_str(&format!("\nGROUP BY {}", groups.join(", ")));
            }
        }
        let order: Vec<String> = self
            .outputs
            .iter()
            .filter(|o| o.sort != Sort::None)
            .map(|o| {
                let by = if o.alias.trim().is_empty() { expr(o) } else { d.ident(o.alias.trim()) };
                if o.sort == Sort::Desc { format!("{by} DESC") } else { by }
            })
            .collect();
        if !order.is_empty() {
            sql.push_str(&format!("\nORDER BY {}", order.join(", ")));
        }
        if kind != DbKind::MsSql
            && let Some(n) = self.limit
        {
            sql.push_str(&format!("\nLIMIT {n}"));
        }
        sql
    }

    fn condition(&self, d: Dialect, f: &Filter, col: String) -> String {
        let numeric = self.numeric(f.table, &f.column);
        let value = |v: &str| {
            let v = v.trim();
            if numeric && v.parse::<f64>().is_ok() { v.to_string() } else { d.string(v) }
        };
        match f.op {
            Op::IsNull | Op::IsNotNull => format!("{col} {}", f.op.label()),
            Op::In => {
                let list: Vec<String> = f.value.split(',').filter(|v| !v.trim().is_empty()).map(value).collect();
                if list.is_empty() { "1 = 0".into() } else { format!("{col} IN ({})", list.join(", ")) }
            }
            op => format!("{col} {} {}", op.label(), value(&f.value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnInfo, ForeignKeyInfo};

    fn column(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: data_type.into(), nullable: true, ..Default::default() }
    }

    fn users() -> TableDetails {
        TableDetails { columns: vec![column("id", "integer"), column("name", "text")], ..Default::default() }
    }

    fn orders() -> TableDetails {
        TableDetails {
            columns: vec![column("id", "integer"), column("user_id", "integer"), column("total", "numeric")],
            foreign_keys: vec![ForeignKeyInfo {
                name: "orders_user".into(),
                columns: vec!["user_id".into()],
                ref_schema: "public".into(),
                ref_table: "users".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn path(name: &str) -> ObjectPath {
        ObjectPath::new("shop", "public", name)
    }

    #[test]
    fn one_table() {
        let mut q = Query::default();
        q.add_table(path("users"), users());
        assert_eq!(q.sql(DbKind::Postgres), "SELECT\n  *\nFROM \"public\".\"users\"");
        q.toggle_output(0, "name");
        q.filters.push(Filter { table: 0, column: "id".into(), op: Op::Gt, value: "10".into() });
        q.filters.push(Filter { table: 0, column: "name".into(), op: Op::Like, value: "A%".into() });
        q.outputs[0].sort = Sort::Desc;
        q.limit = Some(5);
        assert_eq!(
            q.sql(DbKind::Postgres),
            "SELECT\n  \"name\"\nFROM \"public\".\"users\"\nWHERE \"id\" > 10\n  AND \"name\" LIKE 'A%'\nORDER BY \"name\" DESC\nLIMIT 5"
        );
        assert_eq!(
            q.sql(DbKind::MsSql),
            "SELECT TOP 5\n  [name]\nFROM [public].[users]\nWHERE [id] > 10\n  AND [name] LIKE N'A%'\nORDER BY [name] DESC"
        );
    }

    #[test]
    fn joins_on_foreign_keys_and_groups() {
        let mut q = Query::default();
        q.add_table(path("users"), users());
        q.add_table(path("orders"), orders());
        assert_eq!(
            q.joins,
            [Join { left: 1, right: 0, on: vec![("user_id".into(), "id".into())], kind: JoinKind::Inner }]
        );
        q.joins[0].kind = JoinKind::Right;
        q.toggle_output(0, "name");
        q.toggle_output(1, "total");
        q.outputs[1].agg = Some(Agg::Sum);
        q.outputs[1].alias = "spent".into();
        q.outputs[1].sort = Sort::Desc;
        q.filters.push(Filter { table: 1, column: "total".into(), op: Op::In, value: "1, 2,x".into() });
        assert_eq!(
            q.sql(DbKind::MySql),
            "SELECT\n  `users`.`name`,\n  SUM(`orders`.`total`) AS `spent`\nFROM `public`.`users`\n  \
             LEFT JOIN `public`.`orders` ON `orders`.`user_id` = `users`.`id`\n\
             WHERE `orders`.`total` IN (1, 2, 'x')\nGROUP BY `users`.`name`\nORDER BY `spent` DESC"
        );
    }

    #[test]
    fn same_table_twice_and_cross_joins() {
        let mut q = Query::default();
        q.add_table(path("users"), users());
        q.add_table(path("users"), users());
        assert_eq!(q.tables[1].alias, "users_2");
        assert!(q.joins.is_empty());
        assert!(q.sql(DbKind::Sqlite).contains("CROSS JOIN \"public\".\"users\" AS \"users_2\""));
        q.link(1, "id", 0, "id");
        q.link(0, "name", 1, "name");
        assert_eq!(q.joins.len(), 1);
        assert!(q.sql(DbKind::Sqlite).contains(
            "JOIN \"public\".\"users\" AS \"users_2\" ON \"users_2\".\"id\" = \"users\".\"id\" AND \"users_2\".\"name\" = \"users\".\"name\""
        ));
        q.add_table(path("orders"), orders());
        assert_eq!(q.joins.len(), 3, "orders joins both copies of users");
        q.toggle_output(2, "id");
        q.remove_table(0);
        assert_eq!(q.joins.len(), 1);
        assert_eq!((q.joins[0].left, q.joins[0].right), (1, 0));
        assert_eq!(q.outputs[0].table, 1);
        q.filters.push(Filter { table: 0, column: "name".into(), op: Op::IsNull, value: String::new() });
        assert!(q.sql(DbKind::Postgres).contains("WHERE \"users_2\".\"name\" IS NULL"));
    }
}
