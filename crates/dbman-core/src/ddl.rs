//! DDL generation per dialect: CREATE / ALTER / DROP / RENAME of tables and
//! other objects, and templates for new views and routines.

use crate::sql::Dialect;
use crate::{ColumnInfo, DbKind, DbObject, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath, TableDetails};

/// A column in the table editor; `original` is its name when it was loaded
/// (`None` for a new column), so renames can be told from drop + add.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnDef {
    pub col: ColumnInfo,
    pub original: Option<String>,
}

/// Everything the table editor edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDef {
    pub path: ObjectPath,
    pub columns: Vec<ColumnDef>,
    /// Secondary indexes (the primary key comes from the columns).
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub comment: Option<String>,
    /// Name of the primary key constraint, needed to drop it in PostgreSQL.
    pub pk_name: Option<String>,
}

impl TableDef {
    pub fn new(path: ObjectPath) -> Self {
        Self { path, ..Default::default() }
    }

    pub fn from_details(path: ObjectPath, d: &TableDetails) -> Self {
        Self {
            path,
            columns: d.columns.iter().map(|c| ColumnDef { col: c.clone(), original: Some(c.name.clone()) }).collect(),
            // MySQL creates an index for each foreign key, named after it; it goes with the key.
            indexes: d
                .indexes
                .iter()
                .filter(|i| !i.primary && !d.foreign_keys.iter().any(|f| f.name == i.name))
                .cloned()
                .collect(),
            foreign_keys: d.foreign_keys.clone(),
            comment: d.comment.clone(),
            pk_name: d.indexes.iter().find(|i| i.primary).map(|i| i.name.clone()),
        }
    }

    fn pk(&self) -> Vec<&str> {
        self.columns.iter().filter(|c| c.col.primary_key).map(|c| c.col.name.as_str()).collect()
    }
}

/// Statements to run, plus notes for the user (shown as comments in previews).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub statements: Vec<String>,
    pub notes: Vec<String>,
}

impl Plan {
    /// The plan as a script for previews.
    pub fn script(&self) -> String {
        let mut s = String::new();
        for n in &self.notes {
            s.push_str(&format!("-- {n}\n"));
        }
        for st in &self.statements {
            s.push_str(st);
            s.push_str(";\n");
        }
        s
    }
}

/// Frequently used types, offered in the column type picker.
pub fn common_types(kind: DbKind) -> &'static [&'static str] {
    match kind {
        DbKind::Postgres => &[
            "integer",
            "bigint",
            "smallint",
            "numeric(10,2)",
            "real",
            "double precision",
            "boolean",
            "text",
            "varchar(255)",
            "char(1)",
            "uuid",
            "date",
            "time",
            "timestamp",
            "timestamptz",
            "interval",
            "json",
            "jsonb",
            "bytea",
            "inet",
            "text[]",
            "integer[]",
        ],
        DbKind::MySql => &[
            "int",
            "bigint",
            "smallint",
            "tinyint(1)",
            "decimal(10,2)",
            "float",
            "double",
            "varchar(255)",
            "char(1)",
            "text",
            "mediumtext",
            "longtext",
            "date",
            "time",
            "datetime",
            "timestamp",
            "json",
            "blob",
            "binary(16)",
            "enum('a','b')",
        ],
        DbKind::Sqlite => &["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC", "BOOLEAN", "DATE", "DATETIME"],
        DbKind::MsSql => &[
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "bit",
            "decimal(18,2)",
            "float",
            "nvarchar(255)",
            "nvarchar(max)",
            "varchar(255)",
            "nchar(1)",
            "uniqueidentifier",
            "date",
            "time",
            "datetime2",
            "datetimeoffset",
            "varbinary(max)",
            "xml",
            "money",
        ],
        DbKind::DuckDb => &[
            "INTEGER",
            "BIGINT",
            "SMALLINT",
            "HUGEINT",
            "DECIMAL(18,3)",
            "DOUBLE",
            "FLOAT",
            "BOOLEAN",
            "VARCHAR",
            "UUID",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "INTERVAL",
            "BLOB",
            "JSON",
            "INTEGER[]",
            "VARCHAR[]",
            "STRUCT(a INTEGER, b VARCHAR)",
            "MAP(VARCHAR, INTEGER)",
        ],
    }
}

fn idents(d: Dialect, names: &[String]) -> String {
    names.iter().map(|n| d.ident(n)).collect::<Vec<_>>().join(", ")
}

fn is_no_action(a: &str) -> bool {
    a.is_empty() || a.eq_ignore_ascii_case("NO ACTION")
}

fn column_sql(kind: DbKind, c: &ColumnInfo, inline_pk: bool) -> String {
    let d = Dialect(kind);
    let mut s = format!("{} {}", d.ident(&c.name), c.data_type.trim());
    if inline_pk {
        s.push_str(" PRIMARY KEY");
        if c.auto_increment {
            s.push_str(" AUTOINCREMENT");
        }
    }
    let identity = matches!(kind, DbKind::Postgres | DbKind::MsSql) && c.auto_increment;
    if identity {
        s.push_str(if kind == DbKind::MsSql { " IDENTITY(1,1)" } else { " GENERATED BY DEFAULT AS IDENTITY" });
    }
    if !c.nullable && !inline_pk {
        s.push_str(" NOT NULL");
    }
    if let Some(def) = c.default.as_deref().map(str::trim).filter(|d| !d.is_empty())
        && !identity
    {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if kind == DbKind::MySql {
        if c.auto_increment {
            s.push_str(" AUTO_INCREMENT");
        }
        if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
            s.push_str(&format!(" COMMENT {}", d.string(cm)));
        }
    }
    s
}

fn fk_sql(kind: DbKind, db: &str, f: &ForeignKeyInfo) -> String {
    let d = Dialect(kind);
    let target = if kind == DbKind::Sqlite {
        d.ident(&f.ref_table)
    } else {
        d.qualified(&ObjectPath::new(db, &f.ref_schema, &f.ref_table))
    };
    let mut s = String::new();
    if !f.name.is_empty() {
        s.push_str(&format!("CONSTRAINT {} ", d.ident(&f.name)));
    }
    s.push_str(&format!("FOREIGN KEY ({}) REFERENCES {target} ({})", idents(d, &f.columns), idents(d, &f.ref_columns)));
    // DuckDB only supports NO ACTION / RESTRICT.
    if kind == DbKind::DuckDb {
        return s;
    }
    if !is_no_action(&f.on_delete) {
        s.push_str(&format!(" ON DELETE {}", f.on_delete.to_ascii_uppercase()));
    }
    if !is_no_action(&f.on_update) {
        s.push_str(&format!(" ON UPDATE {}", f.on_update.to_ascii_uppercase()));
    }
    s
}

fn create_index(kind: DbKind, table: &ObjectPath, i: &IndexInfo) -> String {
    let d = Dialect(kind);
    let unique = if i.unique { "UNIQUE " } else { "" };
    let cols = idents(d, &i.columns);
    match kind {
        // SQLite qualifies the index, not the table.
        DbKind::Sqlite => format!(
            "CREATE {unique}INDEX {}.{} ON {} ({cols})",
            d.ident(&table.schema),
            d.ident(&i.name),
            d.ident(&table.name)
        ),
        _ => format!("CREATE {unique}INDEX {} ON {} ({cols})", d.ident(&i.name), d.qualified(table)),
    }
}

fn drop_index(kind: DbKind, table: &ObjectPath, name: &str) -> String {
    let d = Dialect(kind);
    match kind {
        DbKind::MySql | DbKind::MsSql => format!("DROP INDEX {} ON {}", d.ident(name), d.qualified(table)),
        _ => format!("DROP INDEX {}.{}", d.ident(&table.schema), d.ident(name)),
    }
}

/// SQL Server keeps comments as `MS_Description` extended properties.
/// `proc` is `sp_addextendedproperty`, `sp_updateextendedproperty` or `sp_dropextendedproperty`.
fn ms_description(proc: &str, table: &ObjectPath, column: Option<&str>, value: Option<&str>) -> String {
    let d = Dialect(DbKind::MsSql);
    let mut s = format!("EXEC sys.{proc} @name = N'MS_Description'");
    if let Some(v) = value {
        s.push_str(&format!(", @value = {}", d.string(v)));
    }
    s.push_str(&format!(
        ", @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}",
        d.string(&table.schema),
        d.string(&table.name)
    ));
    if let Some(c) = column {
        s.push_str(&format!(", @level2type = N'COLUMN', @level2name = {}", d.string(c)));
    }
    s
}

/// The statement that sets, changes or removes a comment in SQL Server.
fn ms_comment(table: &ObjectPath, column: Option<&str>, old: &str, new: &str) -> Option<String> {
    match (old.is_empty(), new.is_empty()) {
        (true, true) => None,
        _ if old == new => None,
        (true, false) => Some(ms_description("sp_addextendedproperty", table, column, Some(new))),
        (false, true) => Some(ms_description("sp_dropextendedproperty", table, column, None)),
        (false, false) => Some(ms_description("sp_updateextendedproperty", table, column, Some(new))),
    }
}

/// Drops the (system-named) default constraint of a SQL Server column, if any.
fn ms_drop_default(table: &ObjectPath, column: &str) -> String {
    let d = Dialect(DbKind::MsSql);
    let t = d.qualified(table);
    format!(
        "DECLARE @df sysname = (SELECT dc.name FROM sys.default_constraints dc JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {}); IF @df IS NOT NULL BEGIN DECLARE @drop nvarchar(max) = N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@df); EXEC sp_executesql @drop END",
        d.string(&t),
        d.string(column),
        t.replace('\'', "''")
    )
}

/// `CREATE TABLE` and its indexes and comments.
pub fn create_table(kind: DbKind, def: &TableDef) -> Plan {
    let d = Dialect(kind);
    let t = d.qualified(&def.path);
    let pk = def.pk();
    // SQLite's AUTOINCREMENT only works on an inline `INTEGER PRIMARY KEY`.
    let inline_pk = kind == DbKind::Sqlite
        && pk.len() == 1
        && def.columns.iter().any(|c| c.col.primary_key && c.col.auto_increment);
    let mut lines: Vec<String> = def
        .columns
        .iter()
        .map(|c| format!("    {}", column_sql(kind, &c.col, inline_pk && c.col.primary_key)))
        .collect();
    if !pk.is_empty() && !inline_pk {
        let names: Vec<String> = pk.iter().map(|s| s.to_string()).collect();
        lines.push(format!("    PRIMARY KEY ({})", idents(d, &names)));
    }
    for f in &def.foreign_keys {
        lines.push(format!("    {}", fk_sql(kind, &def.path.database, f)));
    }
    let mut create = format!("CREATE TABLE {t} (\n{}\n)", lines.join(",\n"));
    let comment = def.comment.as_deref().filter(|c| !c.is_empty());
    if let (DbKind::MySql, Some(c)) = (kind, comment) {
        create.push_str(&format!(" COMMENT = {}", d.string(c)));
    }
    let mut plan = Plan { statements: vec![create], notes: Vec::new() };
    for i in &def.indexes {
        plan.statements.push(create_index(kind, &def.path, i));
    }
    if kind == DbKind::DuckDb && def.columns.iter().any(|c| c.col.auto_increment) {
        plan.notes.push("DuckDB has no auto-increment columns; use a sequence: DEFAULT nextval('seq').".into());
    }
    if kind == DbKind::DuckDb
        && def.foreign_keys.iter().any(|f| !is_no_action(&f.on_delete) || !is_no_action(&f.on_update))
    {
        plan.notes.push("DuckDB foreign keys have no ON DELETE / ON UPDATE actions; they are left out.".into());
    }
    if matches!(kind, DbKind::Postgres | DbKind::DuckDb) {
        if let Some(c) = comment {
            plan.statements.push(format!("COMMENT ON TABLE {t} IS {}", d.string(c)));
        }
        for c in &def.columns {
            if let Some(cm) = c.col.comment.as_deref().filter(|c| !c.is_empty()) {
                plan.statements.push(format!("COMMENT ON COLUMN {t}.{} IS {}", d.ident(&c.col.name), d.string(cm)));
            }
        }
    }
    if kind == DbKind::MsSql {
        if let Some(c) = comment {
            plan.statements.push(ms_description("sp_addextendedproperty", &def.path, None, Some(c)));
        }
        for c in &def.columns {
            if let Some(cm) = c.col.comment.as_deref().filter(|c| !c.is_empty()) {
                plan.statements.push(ms_description("sp_addextendedproperty", &def.path, Some(&c.col.name), Some(cm)));
            }
        }
    }
    if kind == DbKind::Sqlite && def.columns.iter().any(|c| c.col.comment.as_deref().is_some_and(|c| !c.is_empty())) {
        plan.notes.push("SQLite has no column comments; they are not saved.".into());
    }
    plan
}

fn same_index(a: &IndexInfo, b: &IndexInfo) -> bool {
    a.columns == b.columns && a.unique == b.unique
}

fn same_fk(a: &ForeignKeyInfo, b: &ForeignKeyInfo) -> bool {
    let action = |s: &str| if is_no_action(s) { String::new() } else { s.to_ascii_uppercase() };
    a.columns == b.columns
        && a.ref_schema == b.ref_schema
        && a.ref_table == b.ref_table
        && a.ref_columns == b.ref_columns
        && action(&a.on_delete) == action(&b.on_delete)
        && action(&a.on_update) == action(&b.on_update)
}

/// Statements that turn `old` into `new` (same table; rename it with [`rename`]).
pub fn alter_table(kind: DbKind, old: &TableDef, new: &TableDef) -> Plan {
    let d = Dialect(kind);
    let t = d.qualified(&new.path);
    let mut plan = Plan::default();
    let old_col = |name: &str| old.columns.iter().find(|c| c.col.name == name).map(|c| &c.col);
    let kept: Vec<&ColumnDef> =
        new.columns.iter().filter(|c| c.original.as_deref().and_then(old_col).is_some()).collect();
    let dropped: Vec<&ColumnInfo> = old
        .columns
        .iter()
        .map(|c| &c.col)
        .filter(|c| !new.columns.iter().any(|n| n.original.as_deref() == Some(&c.name)))
        .collect();
    let added: Vec<&ColumnDef> = new.columns.iter().filter(|c| c.original.is_none()).collect();
    let modified = |c: &ColumnDef| {
        let o = c.original.as_deref().and_then(old_col).expect("kept column");
        o.data_type.trim() != c.col.data_type.trim()
            || o.nullable != c.col.nullable
            || o.default.as_deref().unwrap_or("").trim() != c.col.default.as_deref().unwrap_or("").trim()
            || o.auto_increment != c.col.auto_increment
    };
    let pk_changed = old.pk().len() != new.pk().len()
        || kept
            .iter()
            .any(|c| c.original.as_deref().and_then(old_col).is_some_and(|o| o.primary_key != c.col.primary_key))
        || added.iter().any(|c| c.col.primary_key);
    let fks_changed = old.foreign_keys.len() != new.foreign_keys.len()
        || new.foreign_keys.iter().any(|f| !old.foreign_keys.iter().any(|o| o.name == f.name && same_fk(o, f)));

    if kind == DbKind::Sqlite {
        let needs_rebuild = pk_changed
            || fks_changed
            || kept.iter().any(|c| modified(c))
            || added.iter().any(|c| {
                c.col.primary_key || (!c.col.nullable && c.col.default.as_deref().unwrap_or("").trim().is_empty())
            });
        if needs_rebuild {
            return rebuild_sqlite(old, new, &kept);
        }
    }

    // DuckDB cannot add or drop constraints on an existing table.
    let pk_changed = if kind == DbKind::DuckDb && (pk_changed || fks_changed) {
        plan.notes.push(
            "DuckDB cannot change the primary key or foreign keys of an existing table; those changes are skipped."
                .into(),
        );
        false
    } else {
        pk_changed
    };

    // MySQL refuses to change a column used by a foreign key, so such keys are
    // dropped first and added again (with renamed columns) afterwards.
    let renames: std::collections::HashMap<&str, &str> = kept
        .iter()
        .filter_map(|c| c.original.as_deref().filter(|o| *o != c.col.name).map(|o| (o, c.col.name.as_str())))
        .collect();
    let touched: Vec<&str> = if kind == DbKind::MySql {
        kept.iter()
            .filter(|c| renames.contains_key(c.original.as_deref().unwrap()) || modified(c))
            .map(|c| c.original.as_deref().unwrap())
            .collect()
    } else {
        Vec::new()
    };
    let rebuilt = |f: &ForeignKeyInfo| f.columns.iter().any(|c| touched.contains(&c.as_str()));
    let renamed_fk = |f: &ForeignKeyInfo| ForeignKeyInfo {
        columns: f.columns.iter().map(|c| renames.get(c.as_str()).map_or(c.clone(), |n| n.to_string())).collect(),
        ..f.clone()
    };

    // Drop foreign keys and indexes that go away or change first, so their columns can change.
    let keep_fks = kind == DbKind::DuckDb;
    for f in old.foreign_keys.iter().filter(|_| !keep_fks) {
        if rebuilt(f) || !new.foreign_keys.iter().any(|n| n.name == f.name && same_fk(f, n)) {
            plan.statements.push(match kind {
                DbKind::MySql => format!("ALTER TABLE {t} DROP FOREIGN KEY {}", d.ident(&f.name)),
                _ => format!("ALTER TABLE {t} DROP CONSTRAINT {}", d.ident(&f.name)),
            });
        }
    }
    for i in &old.indexes {
        if !new.indexes.iter().any(|n| n.name == i.name && same_index(i, n)) {
            plan.statements.push(drop_index(kind, &new.path, &i.name));
        }
    }
    if pk_changed && !old.pk().is_empty() {
        plan.statements.push(match (kind, &old.pk_name) {
            (DbKind::Postgres | DbKind::MsSql, Some(name)) => {
                format!("ALTER TABLE {t} DROP CONSTRAINT {}", d.ident(name))
            }
            _ => format!("ALTER TABLE {t} DROP PRIMARY KEY"),
        });
    }
    for c in &dropped {
        if kind == DbKind::MsSql && c.default.is_some() {
            plan.statements.push(ms_drop_default(&new.path, &c.name));
        }
        plan.statements.push(format!("ALTER TABLE {t} DROP COLUMN {}", d.ident(&c.name)));
    }
    for c in &kept {
        let orig = c.original.as_deref().unwrap();
        let o = old_col(orig).unwrap();
        let renamed = orig != c.col.name;
        match kind {
            DbKind::MySql => {
                let comment_changed = o.comment.as_deref().unwrap_or("") != c.col.comment.as_deref().unwrap_or("");
                if renamed || modified(c) || comment_changed {
                    plan.statements.push(format!(
                        "ALTER TABLE {t} CHANGE COLUMN {} {}",
                        d.ident(orig),
                        column_sql(kind, &c.col, false)
                    ));
                }
            }
            DbKind::MsSql => {
                let col = d.ident(&c.col.name);
                if renamed {
                    let from = format!("{}.{}", d.qualified(&new.path), d.ident(orig));
                    plan.statements.push(format!(
                        "EXEC sp_rename {}, {}, N'COLUMN'",
                        d.string(&from),
                        d.string(&c.col.name)
                    ));
                }
                let (od, nd) =
                    (o.default.as_deref().unwrap_or("").trim(), c.col.default.as_deref().unwrap_or("").trim());
                let retyped = o.data_type.trim() != c.col.data_type.trim() || o.nullable != c.col.nullable;
                // A column with a default constraint cannot change type.
                if !od.is_empty() && (od != nd || retyped) {
                    plan.statements.push(ms_drop_default(&new.path, &c.col.name));
                }
                if retyped {
                    let null = if c.col.nullable { "NULL" } else { "NOT NULL" };
                    plan.statements
                        .push(format!("ALTER TABLE {t} ALTER COLUMN {col} {} {null}", c.col.data_type.trim()));
                }
                if !nd.is_empty() && (od != nd || retyped) {
                    plan.statements.push(format!("ALTER TABLE {t} ADD DEFAULT {nd} FOR {col}"));
                }
                if o.auto_increment != c.col.auto_increment {
                    plan.notes.push(format!(
                        "SQL Server cannot add or remove IDENTITY on the existing column {}; that change is skipped.",
                        c.col.name
                    ));
                }
                let (ocm, ncm) = (o.comment.as_deref().unwrap_or(""), c.col.comment.as_deref().unwrap_or(""));
                if let Some(st) = ms_comment(&new.path, Some(&c.col.name), ocm, ncm) {
                    plan.statements.push(st);
                }
            }
            _ => {
                let col = d.ident(&c.col.name);
                if renamed {
                    plan.statements.push(format!("ALTER TABLE {t} RENAME COLUMN {} TO {col}", d.ident(orig)));
                }
                if !matches!(kind, DbKind::Postgres | DbKind::DuckDb) {
                    continue;
                }
                if o.data_type.trim() != c.col.data_type.trim() {
                    let ty = c.col.data_type.trim();
                    plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty}"));
                }
                if o.auto_increment != c.col.auto_increment && kind == DbKind::Postgres {
                    plan.statements.push(if c.col.auto_increment {
                        format!("ALTER TABLE {t} ALTER COLUMN {col} ADD GENERATED BY DEFAULT AS IDENTITY")
                    } else {
                        format!("ALTER TABLE {t} ALTER COLUMN {col} DROP IDENTITY IF EXISTS")
                    });
                }
                if o.nullable != c.col.nullable {
                    let what = if c.col.nullable { "DROP" } else { "SET" };
                    plan.statements.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {what} NOT NULL"));
                }
                let (od, nd) =
                    (o.default.as_deref().unwrap_or("").trim(), c.col.default.as_deref().unwrap_or("").trim());
                if od != nd && !c.col.auto_increment {
                    plan.statements.push(if nd.is_empty() {
                        format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT")
                    } else {
                        format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {nd}")
                    });
                }
                if o.comment.as_deref().unwrap_or("") != c.col.comment.as_deref().unwrap_or("") {
                    let v = c.col.comment.as_deref().filter(|c| !c.is_empty()).map_or("NULL".into(), |c| d.string(c));
                    plan.statements.push(format!("COMMENT ON COLUMN {t}.{col} IS {v}"));
                }
            }
        }
    }
    for c in &added {
        // SQL Server's ADD has no COLUMN keyword.
        let add = if kind == DbKind::MsSql { "ADD" } else { "ADD COLUMN" };
        plan.statements.push(format!("ALTER TABLE {t} {add} {}", column_sql(kind, &c.col, false)));
        if kind == DbKind::MsSql
            && let Some(cm) = c.col.comment.as_deref().filter(|c| !c.is_empty())
        {
            plan.statements.push(ms_description("sp_addextendedproperty", &new.path, Some(&c.col.name), Some(cm)));
        }
        if matches!(kind, DbKind::Postgres | DbKind::DuckDb)
            && let Some(cm) = c.col.comment.as_deref().filter(|c| !c.is_empty())
        {
            plan.statements.push(format!("COMMENT ON COLUMN {t}.{} IS {}", d.ident(&c.col.name), d.string(cm)));
        }
    }
    if pk_changed && !new.pk().is_empty() {
        let names: Vec<String> = new.pk().iter().map(|s| s.to_string()).collect();
        plan.statements.push(format!("ALTER TABLE {t} ADD PRIMARY KEY ({})", idents(d, &names)));
    }
    for i in &new.indexes {
        if !old.indexes.iter().any(|o| o.name == i.name && same_index(i, o)) {
            plan.statements.push(create_index(kind, &new.path, i));
        }
    }
    for f in new.foreign_keys.iter().filter(|_| !keep_fks) {
        let unchanged = old.foreign_keys.iter().find(|o| o.name == f.name && same_fk(f, o));
        if unchanged.is_none_or(rebuilt) {
            plan.statements.push(format!("ALTER TABLE {t} ADD {}", fk_sql(kind, &new.path.database, &renamed_fk(f))));
        }
    }
    let (oc, nc) = (old.comment.as_deref().unwrap_or(""), new.comment.as_deref().unwrap_or(""));
    if oc != nc {
        match kind {
            DbKind::Postgres | DbKind::DuckDb => {
                let v = if nc.is_empty() { "NULL".into() } else { d.string(nc) };
                plan.statements.push(format!("COMMENT ON TABLE {t} IS {v}"));
            }
            DbKind::MySql => plan.statements.push(format!("ALTER TABLE {t} COMMENT = {}", d.string(nc))),
            DbKind::Sqlite => plan.notes.push("SQLite has no table comments; the comment is not saved.".into()),
            DbKind::MsSql => plan.statements.extend(ms_comment(&new.path, None, oc, nc)),
        }
    }
    plan
}

/// SQLite cannot change a column in place: build a new table, copy the rows,
/// drop the old one and rename the new one.
fn rebuild_sqlite(old: &TableDef, new: &TableDef, kept: &[&ColumnDef]) -> Plan {
    let d = Dialect(DbKind::Sqlite);
    let tmp_name = format!("{}__dbman_new", new.path.name);
    let mut tmp = new.clone();
    tmp.path.name = tmp_name.clone();
    tmp.indexes.clear();
    let mut plan = Plan {
        statements: vec!["PRAGMA foreign_keys = OFF".into()],
        notes: vec![
            "SQLite cannot alter these columns in place, so the table is rebuilt and its rows copied.".into(),
            "Triggers on the table are dropped with it and must be created again.".into(),
        ],
    };
    plan.statements.extend(create_table(DbKind::Sqlite, &tmp).statements);
    let into: Vec<String> = kept.iter().map(|c| c.col.name.clone()).collect();
    let from: Vec<String> = kept.iter().map(|c| c.original.clone().unwrap()).collect();
    if !into.is_empty() {
        plan.statements.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            d.qualified(&tmp.path),
            idents(d, &into),
            idents(d, &from),
            d.qualified(&old.path)
        ));
    }
    plan.statements.push(format!("DROP TABLE {}", d.qualified(&old.path)));
    plan.statements.push(format!("ALTER TABLE {} RENAME TO {}", d.qualified(&tmp.path), d.ident(&new.path.name)));
    for i in &new.indexes {
        plan.statements.push(create_index(DbKind::Sqlite, &new.path, i));
    }
    plan.statements.push("PRAGMA foreign_keys = ON".into());
    plan
}

pub fn create_database(kind: DbKind, name: &str) -> String {
    format!("CREATE DATABASE {}", Dialect(kind).ident(name))
}

pub fn drop_database(kind: DbKind, name: &str) -> String {
    format!("DROP DATABASE {}", Dialect(kind).ident(name))
}

pub fn create_schema(kind: DbKind, name: &str) -> String {
    format!("CREATE SCHEMA {}", Dialect(kind).ident(name))
}

pub fn drop_schema(kind: DbKind, name: &str, cascade: bool) -> String {
    let cascade = cascade && matches!(kind, DbKind::Postgres | DbKind::DuckDb);
    format!("DROP SCHEMA {}{}", Dialect(kind).ident(name), if cascade { " CASCADE" } else { "" })
}

/// `DROP …` for any explorer object.
pub fn drop_object(kind: DbKind, o: &DbObject, cascade: bool) -> String {
    let d = Dialect(kind);
    let q = d.qualified(&o.path);
    let cascade = if cascade && kind == DbKind::Postgres { " CASCADE" } else { "" };
    let what = match o.kind {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::ForeignTable => "FOREIGN TABLE",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Type => "TYPE",
    };
    match (o.kind, kind) {
        (ObjectKind::Function | ObjectKind::Procedure, DbKind::Postgres) => {
            format!("DROP {what} {q}({}){cascade}", o.detail.as_deref().unwrap_or(""))
        }
        (ObjectKind::Function, DbKind::DuckDb) => format!("DROP MACRO {q}"),
        (ObjectKind::Trigger, DbKind::Postgres) => {
            let table = ObjectPath::new(&o.path.database, &o.path.schema, o.detail.as_deref().unwrap_or(""));
            format!("DROP TRIGGER {} ON {}{cascade}", d.ident(&o.path.name), d.qualified(&table))
        }
        _ => format!("DROP {what} {q}{cascade}"),
    }
}

/// Renames a table, view or sequence; `None` if the engine cannot rename this kind.
pub fn rename(kind: DbKind, o: &DbObject, new_name: &str) -> Option<String> {
    let d = Dialect(kind);
    let q = d.qualified(&o.path);
    let to = d.ident(new_name);
    match (kind, o.kind) {
        (DbKind::Postgres, ObjectKind::Table) => Some(format!("ALTER TABLE {q} RENAME TO {to}")),
        (DbKind::Postgres, ObjectKind::View) => Some(format!("ALTER VIEW {q} RENAME TO {to}")),
        (DbKind::Postgres, ObjectKind::MaterializedView) => Some(format!("ALTER MATERIALIZED VIEW {q} RENAME TO {to}")),
        (DbKind::Postgres, ObjectKind::Sequence) => Some(format!("ALTER SEQUENCE {q} RENAME TO {to}")),
        (DbKind::Postgres, ObjectKind::Type) => Some(format!("ALTER TYPE {q} RENAME TO {to}")),
        (DbKind::Postgres, ObjectKind::Function | ObjectKind::Procedure) => {
            let what = if o.kind == ObjectKind::Function { "FUNCTION" } else { "PROCEDURE" };
            Some(format!("ALTER {what} {q}({}) RENAME TO {to}", o.detail.as_deref().unwrap_or("")))
        }
        (DbKind::MySql, ObjectKind::Table | ObjectKind::View) => {
            let target = d.qualified(&ObjectPath::new(&o.path.database, &o.path.schema, new_name));
            Some(format!("RENAME TABLE {q} TO {target}"))
        }
        (DbKind::Sqlite | DbKind::DuckDb, ObjectKind::Table) => Some(format!("ALTER TABLE {q} RENAME TO {to}")),
        (DbKind::DuckDb, ObjectKind::View) => Some(format!("ALTER VIEW {q} RENAME TO {to}")),
        (
            DbKind::MsSql,
            ObjectKind::Table | ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Sequence,
        ) => Some(format!("EXEC sp_rename {}, {}", d.string(&q), d.string(new_name))),
        _ => None,
    }
}

/// Removes every row of a table.
pub fn truncate(kind: DbKind, table: &ObjectPath) -> String {
    let q = Dialect(kind).qualified(table);
    match kind {
        DbKind::Sqlite => format!("DELETE FROM {q}"),
        _ => format!("TRUNCATE TABLE {q}"),
    }
}

/// Script templates for new objects, opened in a console to be filled in.
pub fn template(kind: DbKind, what: ObjectKind, database: &str, schema: &str) -> String {
    let d = Dialect(kind);
    let at = |name: &str| d.qualified(&ObjectPath::new(database, schema, name));
    match (what, kind) {
        (ObjectKind::View, DbKind::MsSql) => {
            format!("CREATE VIEW {} AS\nSELECT *\nFROM {}\nWHERE 1 = 1;\n", at("new_view"), at("some_table"))
        }
        // GO makes the console run each batch whole, semicolons and all.
        (ObjectKind::Function, DbKind::MsSql) => format!(
            "CREATE OR ALTER FUNCTION {}(@a int, @b int)\nRETURNS int\nAS\nBEGIN\n    RETURN @a + @b;\nEND\nGO\n",
            at("new_function")
        ),
        (ObjectKind::Procedure, DbKind::MsSql) => format!(
            "CREATE OR ALTER PROCEDURE {}\n    @id int\nAS\nBEGIN\n    SET NOCOUNT ON;\n    SELECT @id AS id;\nEND\nGO\n",
            at("new_procedure")
        ),
        (ObjectKind::Trigger, DbKind::MsSql) => format!(
            "CREATE OR ALTER TRIGGER {}\nON {}\nAFTER UPDATE\nAS\nBEGIN\n    SET NOCOUNT ON;\n    UPDATE t SET updated_at = SYSDATETIME()\n    FROM {} t JOIN inserted i ON i.id = t.id;\nEND\nGO\n",
            at("new_trigger"),
            at("some_table"),
            at("some_table")
        ),
        (ObjectKind::Type, DbKind::MsSql) => format!("CREATE TYPE {} FROM nvarchar(50) NOT NULL;\n", at("new_type")),
        (ObjectKind::View, _) => {
            format!("CREATE VIEW {} AS\nSELECT *\nFROM {}\nWHERE true;\n", at("new_view"), at("some_table"))
        }
        (ObjectKind::MaterializedView, _) => {
            format!("CREATE MATERIALIZED VIEW {} AS\nSELECT *\nFROM {}\nWITH DATA;\n", at("new_view"), at("some_table"))
        }
        (ObjectKind::Function, DbKind::Postgres) => format!(
            "CREATE OR REPLACE FUNCTION {}(a integer, b integer)\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN a + b;\nEND;\n$$;\n",
            at("new_function")
        ),
        (ObjectKind::Procedure, DbKind::Postgres) => format!(
            "CREATE OR REPLACE PROCEDURE {}(p_id integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    -- statements\n    RAISE NOTICE 'id: %', p_id;\nEND;\n$$;\n",
            at("new_procedure")
        ),
        (ObjectKind::Function, DbKind::MySql) => {
            format!("CREATE FUNCTION {}(a INT, b INT)\nRETURNS INT\nDETERMINISTIC\nRETURN a + b;\n", at("new_function"))
        }
        (ObjectKind::Procedure, DbKind::MySql) => {
            format!("CREATE PROCEDURE {}(IN p_id INT)\nBEGIN\n    SELECT p_id;\nEND;\n", at("new_procedure"))
        }
        (ObjectKind::Trigger, DbKind::Postgres) => format!(
            "CREATE OR REPLACE FUNCTION {}()\nRETURNS trigger\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    NEW.updated_at := now();\n    RETURN NEW;\nEND;\n$$;\n\nCREATE TRIGGER new_trigger\nBEFORE UPDATE ON {}\nFOR EACH ROW EXECUTE FUNCTION {}();\n",
            at("new_trigger_fn"),
            at("some_table"),
            at("new_trigger_fn")
        ),
        (ObjectKind::Trigger, DbKind::MySql) => format!(
            "CREATE TRIGGER {}\nBEFORE UPDATE ON {}\nFOR EACH ROW\nSET NEW.updated_at = NOW();\n",
            at("new_trigger"),
            at("some_table")
        ),
        (ObjectKind::Trigger, DbKind::Sqlite) => format!(
            "CREATE TRIGGER {}\nAFTER UPDATE ON {}\nBEGIN\n    UPDATE {} SET updated_at = CURRENT_TIMESTAMP WHERE rowid = NEW.rowid;\nEND;\n",
            at("new_trigger"),
            d.ident("some_table"),
            d.ident("some_table")
        ),
        (ObjectKind::Function, DbKind::DuckDb) => {
            format!("CREATE OR REPLACE MACRO {}(a, b) AS a + b;\n", at("new_macro"))
        }
        (ObjectKind::Sequence, _) => format!("CREATE SEQUENCE {} START WITH 1 INCREMENT BY 1;\n", at("new_sequence")),
        (ObjectKind::Type, _) => format!("CREATE TYPE {} AS ENUM ('a', 'b');\n", at("new_type")),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn users(kind: DbKind) -> TableDef {
        let schema = if kind == DbKind::Postgres { "public" } else { "app" };
        let mut def = TableDef::new(ObjectPath::new("app", schema, "users"));
        let mut id = col("id", if kind == DbKind::Sqlite { "INTEGER" } else { "integer" });
        id.primary_key = true;
        id.nullable = false;
        id.auto_increment = true;
        let mut email = col("email", "varchar(100)");
        email.nullable = false;
        email.comment = Some("login".into());
        let mut team = col("team_id", "integer");
        team.default = Some("1".into());
        def.columns = [id, email, team].into_iter().map(|c| ColumnDef { col: c, original: None }).collect();
        def.indexes.push(IndexInfo {
            name: "users_email_key".into(),
            columns: vec!["email".into()],
            unique: true,
            ..Default::default()
        });
        def.foreign_keys.push(ForeignKeyInfo {
            name: "users_team_fk".into(),
            columns: vec!["team_id".into()],
            ref_schema: schema.into(),
            ref_table: "teams".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "SET NULL".into(),
        });
        def
    }

    #[test]
    fn create_postgres() {
        let p = create_table(DbKind::Postgres, &users(DbKind::Postgres));
        assert_eq!(
            p.statements[0],
            "CREATE TABLE \"public\".\"users\" (\n    \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n    \"email\" varchar(100) NOT NULL,\n    \"team_id\" integer DEFAULT 1,\n    PRIMARY KEY (\"id\"),\n    CONSTRAINT \"users_team_fk\" FOREIGN KEY (\"team_id\") REFERENCES \"public\".\"teams\" (\"id\") ON DELETE SET NULL\n)"
        );
        assert_eq!(p.statements[1], "CREATE UNIQUE INDEX \"users_email_key\" ON \"public\".\"users\" (\"email\")");
        assert_eq!(p.statements[2], "COMMENT ON COLUMN \"public\".\"users\".\"email\" IS 'login'");
    }

    #[test]
    fn create_mysql_and_sqlite() {
        let m = create_table(DbKind::MySql, &users(DbKind::MySql));
        assert!(m.statements[0].contains("`id` integer NOT NULL AUTO_INCREMENT"), "{}", m.statements[0]);
        assert!(m.statements[0].contains("`email` varchar(100) NOT NULL COMMENT 'login'"));
        let s = create_table(DbKind::Sqlite, &users(DbKind::Sqlite));
        assert!(s.statements[0].contains("\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"), "{}", s.statements[0]);
        assert!(s.statements[0].contains("REFERENCES \"teams\" (\"id\")"));
        assert!(!s.statements[0].contains("    PRIMARY KEY"));
        assert_eq!(s.statements[1], "CREATE UNIQUE INDEX \"app\".\"users_email_key\" ON \"users\" (\"email\")");
        assert_eq!(s.notes.len(), 1);
    }

    fn loaded(kind: DbKind) -> TableDef {
        let mut d = users(kind);
        for c in &mut d.columns {
            c.original = Some(c.col.name.clone());
        }
        d.pk_name = Some("users_pkey".into());
        d
    }

    #[test]
    fn alter_postgres() {
        let old = loaded(DbKind::Postgres);
        let mut new = old.clone();
        new.columns[1].col.name = "mail".into();
        new.columns[1].col.data_type = "text".into();
        new.columns[2].col.nullable = false;
        new.columns[2].col.default = None;
        new.columns.push(ColumnDef { col: col("age", "int"), original: None });
        new.indexes.clear();
        new.comment = Some("people".into());
        let p = alter_table(DbKind::Postgres, &old, &new);
        let t = "ALTER TABLE \"public\".\"users\"";
        assert_eq!(
            p.statements,
            [
                "DROP INDEX \"public\".\"users_email_key\"".to_string(),
                format!("{t} RENAME COLUMN \"email\" TO \"mail\""),
                format!("{t} ALTER COLUMN \"mail\" TYPE text USING \"mail\"::text"),
                format!("{t} ALTER COLUMN \"team_id\" SET NOT NULL"),
                format!("{t} ALTER COLUMN \"team_id\" DROP DEFAULT"),
                format!("{t} ADD COLUMN \"age\" int"),
                "COMMENT ON TABLE \"public\".\"users\" IS 'people'".to_string(),
            ]
        );
    }

    #[test]
    fn alter_mysql_uses_change_column() {
        let old = loaded(DbKind::MySql);
        let mut new = old.clone();
        new.columns[1].col.name = "mail".into();
        new.columns.remove(2);
        new.foreign_keys.clear();
        let p = alter_table(DbKind::MySql, &old, &new);
        assert_eq!(
            p.statements,
            [
                "ALTER TABLE `app`.`users` DROP FOREIGN KEY `users_team_fk`",
                "ALTER TABLE `app`.`users` DROP COLUMN `team_id`",
                "ALTER TABLE `app`.`users` CHANGE COLUMN `email` `mail` varchar(100) NOT NULL COMMENT 'login'",
            ]
        );
    }

    #[test]
    fn alter_primary_key() {
        let old = loaded(DbKind::Postgres);
        let mut new = old.clone();
        new.columns[1].col.primary_key = true;
        let p = alter_table(DbKind::Postgres, &old, &new);
        assert_eq!(
            p.statements,
            [
                "ALTER TABLE \"public\".\"users\" DROP CONSTRAINT \"users_pkey\"",
                "ALTER TABLE \"public\".\"users\" ADD PRIMARY KEY (\"id\", \"email\")",
            ]
        );
    }

    #[test]
    fn sqlite_rebuilds_for_type_changes_but_not_for_renames() {
        let old = loaded(DbKind::Sqlite);
        let mut renamed = old.clone();
        renamed.columns[1].col.name = "mail".into();
        let p = alter_table(DbKind::Sqlite, &old, &renamed);
        assert_eq!(p.statements, ["ALTER TABLE \"app\".\"users\" RENAME COLUMN \"email\" TO \"mail\""]);

        let mut retyped = renamed.clone();
        retyped.columns[2].col.data_type = "TEXT".into();
        let p = alter_table(DbKind::Sqlite, &old, &retyped);
        assert_eq!(p.statements[0], "PRAGMA foreign_keys = OFF");
        assert!(p.statements[1].starts_with("CREATE TABLE \"app\".\"users__dbman_new\""));
        assert_eq!(
            p.statements[2],
            "INSERT INTO \"app\".\"users__dbman_new\" (\"id\", \"mail\", \"team_id\") SELECT \"id\", \"email\", \"team_id\" FROM \"app\".\"users\""
        );
        assert_eq!(p.statements[3], "DROP TABLE \"app\".\"users\"");
        assert_eq!(p.statements[4], "ALTER TABLE \"app\".\"users__dbman_new\" RENAME TO \"users\"");
        assert!(p.statements[5].starts_with("CREATE UNIQUE INDEX"));
        assert_eq!(p.statements.last().unwrap(), "PRAGMA foreign_keys = ON");
    }

    #[test]
    fn unchanged_table_has_no_statements() {
        for kind in DbKind::ALL {
            let t = loaded(kind);
            assert!(alter_table(kind, &t, &t).statements.is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn drops_and_renames() {
        let f = DbObject {
            kind: ObjectKind::Function,
            path: ObjectPath::new("db", "public", "add"),
            detail: Some("a integer, b integer".into()),
            comment: None,
        };
        assert_eq!(
            drop_object(DbKind::Postgres, &f, true),
            "DROP FUNCTION \"public\".\"add\"(a integer, b integer) CASCADE"
        );
        let trg = DbObject {
            kind: ObjectKind::Trigger,
            path: ObjectPath::new("db", "public", "touch"),
            detail: Some("orders".into()),
            comment: None,
        };
        assert_eq!(drop_object(DbKind::Postgres, &trg, false), "DROP TRIGGER \"touch\" ON \"public\".\"orders\"");
        let t = DbObject {
            kind: ObjectKind::Table,
            path: ObjectPath::new("shop", "shop", "a"),
            detail: None,
            comment: None,
        };
        assert_eq!(rename(DbKind::MySql, &t, "b").unwrap(), "RENAME TABLE `shop`.`a` TO `shop`.`b`");
        assert_eq!(truncate(DbKind::Sqlite, &t.path), "DELETE FROM \"shop\".\"a\"");
        assert!(rename(DbKind::Sqlite, &trg, "x").is_none());
    }
}
