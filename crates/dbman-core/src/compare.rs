//! Comparison of two schemas, and of the rows of tables, with the scripts
//! that make the target match the source.
//!
//! Tables are compared by structure (columns, keys, indexes, comments), other
//! objects by their definitions. Rows are matched by primary key: the target's
//! rows are hashed per column, then the source's rows are streamed past them.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::ddl::{self, ColumnDef, Plan, TableDef};
use crate::import::{self, ColumnMap};
use crate::sql::Dialect;
use crate::{Cell, DbKind, DbObject, ObjectKind, ObjectPath, TableDetails, dml, result};

/// One object of a schema as read for a comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaObject {
    pub object: DbObject,
    /// Structure of tables.
    pub details: Option<TableDetails>,
    /// The definition as the server reports it, or why it could not be read.
    pub ddl: Result<String, String>,
}

/// The objects of one schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub database: String,
    pub schema: String,
    pub objects: Vec<SchemaObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    /// Only in the source: created in the target.
    OnlySource,
    Changed,
    /// Only in the target: dropped if chosen.
    OnlyTarget,
    Same,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::OnlySource => "new",
            Status::Changed => "changed",
            Status::OnlyTarget => "only in target",
            Status::Same => "same",
        }
    }
}

/// One object compared.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDiff {
    pub kind: ObjectKind,
    pub name: String,
    pub detail: Option<String>,
    pub status: Status,
    /// What differs, one line each.
    pub changes: Vec<String>,
    pub source_ddl: Option<String>,
    pub target_ddl: Option<String>,
    /// Statements that remove the target's object (run before everything else).
    pub drop: Vec<String>,
    /// Statements that create or alter the object.
    pub apply: Vec<String>,
    pub notes: Vec<String>,
    /// Tables this one references (for the order of CREATE TABLE).
    pub references: Vec<String>,
}

impl ObjectDiff {
    /// The statements of this object alone, for previews.
    pub fn plan(&self) -> Plan {
        Plan { statements: self.drop.iter().chain(&self.apply).cloned().collect(), notes: self.notes.clone() }
    }
}

/// Moves names from the source schema to the target schema.
struct Retarget {
    kind: DbKind,
    from_db: String,
    from_schema: String,
    to_db: String,
    to_schema: String,
}

impl Retarget {
    fn new(kind: DbKind, source: &Snapshot, target: &Snapshot) -> Self {
        Self {
            kind,
            from_db: source.database.clone(),
            from_schema: source.schema.clone(),
            to_db: target.database.clone(),
            to_schema: target.schema.clone(),
        }
    }

    fn schema(&self, s: &str) -> String {
        if s == self.from_schema {
            self.to_schema.clone()
        } else if s == self.from_db {
            self.to_db.clone()
        } else {
            s.to_string()
        }
    }

    /// Replaces `source_schema.` qualifiers in SQL text.
    fn sql(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (from, to) in [(&self.from_schema, &self.to_schema), (&self.from_db, &self.to_db)] {
            if from != to && !from.is_empty() {
                out = replace_qualifier(&out, Dialect(self.kind), from, to);
            }
        }
        out
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `from.` → `to.`, quoted or bare (whole words only).
fn replace_qualifier(s: &str, d: Dialect, from: &str, to: &str) -> String {
    let quoted = format!("{}.", d.ident(from));
    let mut out = s.replace(&quoted, &format!("{}.", d.ident(to)));
    if !from.chars().all(is_ident_char) {
        return out;
    }
    let bare = format!("{from}.");
    let mut result = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find(&bare) {
        let before = rest[..i].chars().last().or_else(|| result.chars().last());
        result.push_str(&rest[..i]);
        if before.is_some_and(|c| is_ident_char(c) || c == '.' || c == '"' || c == '`' || c == ']') {
            result.push_str(&bare);
        } else {
            result.push_str(&format!("{to}."));
        }
        rest = &rest[i + bare.len()..];
    }
    result.push_str(rest);
    out = result;
    out
}

/// Definitions compared without layout, a trailing `;` or MySQL's DEFINER.
fn normalize(s: &str) -> String {
    let s = trim_statement(s);
    s.split_whitespace().filter(|w| !w.starts_with("DEFINER=")).collect::<Vec<_>>().join(" ")
}

/// A statement without a trailing `;` or SQL Server `GO` line.
fn trim_statement(s: &str) -> String {
    let mut s = s.trim();
    loop {
        let t = s.trim_end_matches(';').trim_end();
        let t = match t.rsplit_once('\n') {
            Some((head, last)) if last.trim().eq_ignore_ascii_case("go") => head.trim_end(),
            _ => t,
        };
        if t.len() == s.len() {
            return t.to_string();
        }
        s = t;
    }
}

/// Objects are created in this order and dropped in the reverse one.
fn rank(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Type => 0,
        ObjectKind::Sequence => 1,
        ObjectKind::Table | ObjectKind::ForeignTable => 2,
        ObjectKind::Function => 3,
        ObjectKind::Procedure => 4,
        ObjectKind::View => 5,
        ObjectKind::MaterializedView => 6,
        ObjectKind::Trigger => 7,
    }
}

fn key(o: &DbObject) -> (ObjectKind, String, String) {
    let detail = match o.kind {
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger => o.detail.clone().unwrap_or_default(),
        _ => String::new(),
    };
    (o.kind, o.path.name.clone(), detail)
}

/// Compares the objects of two schemas of the same kind of database.
pub fn compare(kind: DbKind, source: &Snapshot, target: &Snapshot) -> Vec<ObjectDiff> {
    let rt = Retarget::new(kind, source, target);
    let targets: HashMap<_, &SchemaObject> = target.objects.iter().map(|o| (key(&o.object), o)).collect();
    let mut matched = HashSet::new();
    let mut out = Vec::new();
    for s in &source.objects {
        let k = key(&s.object);
        match targets.get(&k) {
            Some(t) => {
                out.push(compare_pair(kind, &rt, s, t));
                matched.insert(k);
            }
            None => out.push(only_source(kind, &rt, target, s)),
        }
    }
    for t in &target.objects {
        if !matched.contains(&key(&t.object)) {
            out.push(only_target(kind, t));
        }
    }
    out.sort_by(|a, b| (rank(a.kind), &a.name).cmp(&(rank(b.kind), &b.name)));
    out
}

fn base(o: &SchemaObject, status: Status) -> ObjectDiff {
    ObjectDiff {
        kind: o.object.kind,
        name: o.object.path.name.clone(),
        detail: o.object.detail.clone(),
        status,
        changes: Vec::new(),
        source_ddl: None,
        target_ddl: None,
        drop: Vec::new(),
        apply: Vec::new(),
        notes: Vec::new(),
        references: Vec::new(),
    }
}

fn shown(ddl: &Result<String, String>) -> String {
    match ddl {
        Ok(s) => s.clone(),
        Err(e) => format!("-- Could not read the definition: {e}"),
    }
}

/// The source table's definition, moved to the target schema.
fn source_def(rt: &Retarget, path: ObjectPath, d: &TableDetails) -> TableDef {
    let mut def = TableDef::from_details(path, d);
    for c in &mut def.columns {
        c.col.default = c.col.default.as_deref().map(|v| rt.sql(v));
    }
    for f in &mut def.foreign_keys {
        f.ref_schema = rt.schema(&f.ref_schema);
    }
    def
}

fn target_path(target: &Snapshot, name: &str) -> ObjectPath {
    ObjectPath::new(&target.database, &target.schema, name)
}

fn references(d: &TableDetails) -> Vec<String> {
    d.foreign_keys.iter().map(|f| f.ref_table.clone()).collect()
}

fn only_source(kind: DbKind, rt: &Retarget, target: &Snapshot, s: &SchemaObject) -> ObjectDiff {
    let mut diff = base(s, Status::OnlySource);
    diff.source_ddl = Some(shown(&s.ddl));
    diff.changes.push(format!("Create {} {}", s.object.kind.label(), s.object.path.name));
    match (&s.details, &s.ddl) {
        // These servers' own CREATE TABLE keeps engine options the catalog does not show.
        (Some(d), Ok(ddl)) if matches!(kind, DbKind::MySql | DbKind::ClickHouse) => {
            diff.apply.push(trim_statement(&rt.sql(ddl)));
            diff.references = references(d);
        }
        (Some(d), _) => {
            let plan = ddl::create_table(kind, &source_def(rt, target_path(target, &s.object.path.name), d));
            diff.apply = plan.statements;
            diff.notes = plan.notes;
            diff.references = references(d);
        }
        (None, Ok(ddl)) => diff.apply.push(trim_statement(&rt.sql(ddl))),
        (None, Err(e)) => {
            diff.notes.push(format!("{} is skipped: its definition could not be read ({e}).", s.object.path.name))
        }
    }
    diff
}

fn only_target(kind: DbKind, t: &SchemaObject) -> ObjectDiff {
    let mut diff = base(t, Status::OnlyTarget);
    diff.target_ddl = Some(shown(&t.ddl));
    diff.changes.push(format!("Drop {} {}", t.object.kind.label(), t.object.path.name));
    diff.drop.push(ddl::drop_object(kind, &t.object, false));
    diff
}

fn compare_pair(kind: DbKind, rt: &Retarget, s: &SchemaObject, t: &SchemaObject) -> ObjectDiff {
    let mut diff = base(s, Status::Same);
    diff.source_ddl = Some(shown(&s.ddl));
    diff.target_ddl = Some(shown(&t.ddl));
    if let (Some(sd), Some(td)) = (&s.details, &t.details)
        && s.object.kind == ObjectKind::Table
    {
        let old = TableDef::from_details(t.object.path.clone(), td);
        let mut new = source_def(rt, t.object.path.clone(), sd);
        for c in &mut new.columns {
            c.original = old.columns.iter().any(|o| o.col.name == c.col.name).then(|| c.col.name.clone());
        }
        new.pk_name = old.pk_name.clone();
        diff.changes = table_changes(&old, &new);
        if !diff.changes.is_empty() {
            diff.status = Status::Changed;
            let plan = ddl::alter_table(kind, &old, &new);
            diff.apply = plan.statements;
            diff.notes = plan.notes;
            diff.references = references(sd);
        }
        return diff;
    }
    match (&s.ddl, &t.ddl) {
        (Ok(a), Ok(b)) => {
            let a = rt.sql(a);
            if normalize(&a) == normalize(b) {
                return diff;
            }
            diff.status = Status::Changed;
            diff.changes.push("The definition differs".into());
            if s.object.kind == ObjectKind::Sequence {
                diff.notes.push(format!(
                    "Sequence {} differs; it is left as it is (change it with ALTER SEQUENCE).",
                    s.object.path.name
                ));
            } else {
                diff.drop.push(ddl::drop_object(kind, &t.object, false));
                diff.apply.push(trim_statement(&a));
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            diff.status = Status::Changed;
            diff.changes.push(format!("The definition could not be read: {e}"));
            diff.notes.push(format!("{} is skipped: its definition could not be read.", s.object.path.name));
        }
    }
    diff
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("").trim()
}

fn null_text(nullable: bool) -> &'static str {
    if nullable { "NULL" } else { "NOT NULL" }
}

fn pk(def: &TableDef) -> Vec<&str> {
    def.columns.iter().filter(|c| c.col.primary_key).map(|c| c.col.name.as_str()).collect()
}

/// What turns `old` (the target) into `new` (the source), for people.
fn table_changes(old: &TableDef, new: &TableDef) -> Vec<String> {
    let mut out = Vec::new();
    let find = |cols: &[ColumnDef], name: &str| cols.iter().find(|c| c.col.name == name).map(|c| c.col.clone());
    for c in &new.columns {
        let n = &c.col;
        let Some(o) = find(&old.columns, &n.name) else {
            out.push(format!("Add column {} {}", n.name, n.data_type));
            continue;
        };
        if o.data_type.trim() != n.data_type.trim() {
            out.push(format!("Column {}: type {} → {}", n.name, o.data_type, n.data_type));
        }
        if o.nullable != n.nullable {
            out.push(format!("Column {}: {} → {}", n.name, null_text(o.nullable), null_text(n.nullable)));
        }
        if opt(&o.default) != opt(&n.default) {
            let show = |s: &str| if s.is_empty() { "none".to_string() } else { s.to_string() };
            out.push(format!("Column {}: default {} → {}", n.name, show(opt(&o.default)), show(opt(&n.default))));
        }
        if o.auto_increment != n.auto_increment {
            let on = if n.auto_increment { "becomes" } else { "is no longer" };
            out.push(format!("Column {} {on} auto-increment", n.name));
        }
        if opt(&o.comment) != opt(&n.comment) {
            out.push(format!("Column {}: comment changed", n.name));
        }
    }
    for c in &old.columns {
        if find(&new.columns, &c.col.name).is_none() {
            out.push(format!("Drop column {}", c.col.name));
        }
    }
    let (op, np) = (pk(old), pk(new));
    if op != np {
        out.push(format!("Primary key ({}) → ({})", op.join(", "), np.join(", ")));
    }
    for i in &new.indexes {
        match old.indexes.iter().find(|o| o.name == i.name) {
            None => out.push(format!("Add index {} ({})", i.name, i.columns.join(", "))),
            Some(o) if !ddl::same_index(o, i) => {
                out.push(format!("Change index {}: ({}) → ({})", i.name, o.columns.join(", "), i.columns.join(", ")))
            }
            Some(_) => {}
        }
    }
    for i in &old.indexes {
        if !new.indexes.iter().any(|n| n.name == i.name) {
            out.push(format!("Drop index {}", i.name));
        }
    }
    for f in &new.foreign_keys {
        match old.foreign_keys.iter().find(|o| o.name == f.name) {
            None => out.push(format!("Add foreign key {} → {}", f.name, f.ref_table)),
            Some(o) if !ddl::same_fk(o, f) => out.push(format!("Change foreign key {}", f.name)),
            Some(_) => {}
        }
    }
    for f in &old.foreign_keys {
        if !new.foreign_keys.iter().any(|n| n.name == f.name) {
            out.push(format!("Drop foreign key {}", f.name));
        }
    }
    if opt(&old.comment) != opt(&new.comment) {
        out.push("Table comment changed".into());
    }
    out
}

/// Orders items so that each comes after the ones it references (when it can:
/// cycles keep their original order). `refs[i]` are indexes into the items.
fn dependency_order(refs: &[Vec<usize>]) -> Vec<usize> {
    let mut order = Vec::with_capacity(refs.len());
    let mut state = vec![0u8; refs.len()]; // 0 new, 1 visiting, 2 done
    fn visit(i: usize, refs: &[Vec<usize>], state: &mut [u8], order: &mut Vec<usize>) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &r in &refs[i] {
            if r != i {
                visit(r, refs, state, order);
            }
        }
        state[i] = 2;
        order.push(i);
    }
    for i in 0..refs.len() {
        visit(i, refs, &mut state, &mut order);
    }
    order
}

/// The script for the chosen objects: drops first (dependents before what
/// they use), then creates and alters (what is used before its dependents).
pub fn sync_plan(diffs: &[&ObjectDiff]) -> Plan {
    let mut plan = Plan::default();
    let mut drops: Vec<&ObjectDiff> = diffs.iter().copied().filter(|d| !d.drop.is_empty()).collect();
    drops.sort_by_key(|d| std::cmp::Reverse(rank(d.kind)));
    for d in drops {
        plan.statements.extend(d.drop.iter().cloned());
    }
    let mut applies: Vec<&ObjectDiff> = diffs.iter().copied().filter(|d| !d.apply.is_empty()).collect();
    applies.sort_by_key(|d| (rank(d.kind), d.status != Status::OnlySource));
    // New tables in foreign key order.
    let created: Vec<&ObjectDiff> =
        applies.iter().copied().filter(|d| d.kind == ObjectKind::Table && d.status == Status::OnlySource).collect();
    let refs: Vec<Vec<usize>> = created
        .iter()
        .map(|d| d.references.iter().filter_map(|r| created.iter().position(|c| &c.name == r)).collect())
        .collect();
    let order = dependency_order(&refs);
    let mut tables_done = false;
    for d in &applies {
        if d.kind == ObjectKind::Table && d.status == Status::OnlySource {
            if !tables_done {
                for &i in &order {
                    plan.statements.extend(created[i].apply.iter().cloned());
                }
                tables_done = true;
            }
            continue;
        }
        plan.statements.extend(d.apply.iter().cloned());
    }
    for d in diffs {
        for n in &d.notes {
            if !plan.notes.contains(n) {
                plan.notes.push(n.clone());
            }
        }
    }
    plan
}

/// A script to read or run in a console: SQL Server batches end with `GO`,
/// since routine bodies contain `;`.
pub fn script(kind: DbKind, plan: &Plan) -> String {
    let mut s = String::new();
    for n in &plan.notes {
        s.push_str(&format!("-- {n}\n"));
    }
    if !plan.notes.is_empty() {
        s.push('\n');
    }
    for st in &plan.statements {
        s.push_str(st);
        s.push_str(if kind == DbKind::MsSql { "\nGO\n" } else { ";\n" });
    }
    s
}

/// Whether a script's statements can run in one transaction: DDL commits on
/// its own in MySQL, and ClickHouse has no transactions.
pub fn transactional_ddl(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::MsSql | DbKind::DuckDb)
}

// ---------------------------------------------------------------- data

/// How the rows of a source table are matched to a target table.
#[derive(Debug, Clone, PartialEq)]
pub struct DataSpec {
    /// The target table, where statements go.
    pub table: ObjectPath,
    /// Columns in both tables, as the target defines them.
    pub columns: Vec<ColumnMap>,
    /// Indexes into `columns` of the primary key.
    pub key: Vec<usize>,
}

impl DataSpec {
    /// Matches columns by name; both tables need the same primary key.
    pub fn new(table: ObjectPath, source: &TableDetails, target: &TableDetails) -> Result<Self, String> {
        let key_names = source.primary_key();
        if key_names.is_empty() {
            return Err("No primary key in the source table".into());
        }
        let mut target_key = target.primary_key();
        let mut sorted = key_names.clone();
        sorted.sort_unstable();
        target_key.sort_unstable();
        if sorted != target_key {
            return Err("The primary keys differ".into());
        }
        let columns: Vec<ColumnMap> = source
            .columns
            .iter()
            .filter_map(|s| target.columns.iter().find(|t| t.name == s.name))
            .enumerate()
            .map(|(i, t)| ColumnMap {
                column: t.name.clone(),
                source: Some(i),
                numeric: result::is_numeric_type(&t.data_type),
                binary: import::is_binary_type(&t.data_type),
                auto_increment: t.auto_increment,
            })
            .collect();
        let key = key_names.iter().filter_map(|k| columns.iter().position(|c| c.column == *k)).collect();
        Ok(Self { table, columns, key })
    }

    pub fn names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.column.as_str()).collect()
    }
}

/// A row that differs, for previews.
#[derive(Debug, Clone, PartialEq)]
pub struct RowSample {
    /// The key as `id = 5`.
    pub key: String,
    pub status: Status,
    /// Columns that differ (changed rows).
    pub columns: Vec<String>,
}

/// The outcome of comparing the rows of one table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DataDiff {
    pub table: ObjectPath,
    pub source_rows: u64,
    pub target_rows: u64,
    pub same: u64,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    /// The first differing rows.
    pub samples: Vec<RowSample>,
    pub deletes: Vec<String>,
    pub updates: Vec<String>,
    /// INSERTs, with what SQL Server and PostgreSQL need around explicit ids.
    pub inserts: Vec<String>,
}

impl DataDiff {
    pub fn differs(&self) -> bool {
        self.inserted + self.updated + self.deleted > 0
    }
}

/// Differing rows kept for previews.
const SAMPLES: usize = 500;
/// Keys per `DELETE … WHERE key IN (…)`.
const DELETE_BATCH: usize = 500;

/// ClickHouse mutations run in the background unless asked to wait.
const CH_SYNC: &str = " SETTINGS mutations_sync = 2";

fn hash_cell(c: &Cell) -> u64 {
    let mut h = DefaultHasher::new();
    c.hash(&mut h);
    h.finish()
}

/// Matches rows by key: feed every target row, then every source row, then [`finish`](Self::finish).
pub struct DataDiffer {
    kind: DbKind,
    spec: DataSpec,
    target: HashMap<Vec<Cell>, Vec<u64>>,
    diff: DataDiff,
    inserts: Vec<Vec<Cell>>,
}

impl DataDiffer {
    pub fn new(kind: DbKind, spec: DataSpec) -> Self {
        let diff = DataDiff { table: spec.table.clone(), ..Default::default() };
        Self { kind, spec, target: HashMap::new(), diff, inserts: Vec::new() }
    }

    fn key_of(&self, row: &[Cell]) -> Vec<Cell> {
        self.spec.key.iter().map(|&k| row[k].clone()).collect()
    }

    fn key_text(&self, key: &[Cell]) -> String {
        let d = Dialect(self.kind);
        self.spec
            .key
            .iter()
            .zip(key)
            .map(|(&k, c)| {
                let col = &self.spec.columns[k];
                format!("{} = {}", col.column, dml::literal(d, c, col.numeric))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn sample(&mut self, key: &[Cell], status: Status, columns: Vec<String>) {
        if self.diff.samples.len() < SAMPLES {
            let key = self.key_text(key);
            self.diff.samples.push(RowSample { key, status, columns });
        }
    }

    /// Rows fed so far, both sides.
    pub fn rows_read(&self) -> u64 {
        self.diff.source_rows + self.diff.target_rows
    }

    /// A target row, with one cell per spec column.
    pub fn add_target(&mut self, row: Vec<Cell>) {
        self.diff.target_rows += 1;
        let key = self.key_of(&row);
        self.target.insert(key, row.iter().map(hash_cell).collect());
    }

    /// A source row, with one cell per spec column.
    pub fn add_source(&mut self, row: Vec<Cell>) {
        self.diff.source_rows += 1;
        let key = self.key_of(&row);
        let Some(hashes) = self.target.remove(&key) else {
            self.diff.inserted += 1;
            self.sample(&key, Status::OnlySource, Vec::new());
            self.inserts.push(row);
            return;
        };
        let changed: Vec<usize> =
            (0..row.len()).filter(|&i| !self.spec.key.contains(&i) && hash_cell(&row[i]) != hashes[i]).collect();
        if changed.is_empty() {
            self.diff.same += 1;
            return;
        }
        self.diff.updated += 1;
        let names = changed.iter().map(|&i| self.spec.columns[i].column.clone()).collect();
        self.sample(&key, Status::Changed, names);
        let d = Dialect(self.kind);
        let col = |i: usize| dml::Value::new(&self.spec.columns[i].column, &row[i], self.spec.columns[i].numeric);
        let set: Vec<dml::Value<'_>> = changed.iter().map(|&i| col(i)).collect();
        let key: Vec<dml::Value<'_>> = self.spec.key.iter().map(|&i| col(i)).collect();
        let sql = if self.kind == DbKind::ClickHouse {
            let assignments = set
                .iter()
                .map(|v| format!("{} = {}", d.ident(v.column), dml::literal(d, v.cell, v.numeric)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "ALTER TABLE {} UPDATE {assignments} WHERE {}{CH_SYNC}",
                d.qualified(&self.spec.table),
                dml::condition(d, &key)
            )
        } else {
            dml::update(d, &self.spec.table, &set, &key)
        };
        self.diff.updates.push(sql);
    }

    /// Rows still unmatched in the target are deleted; the statements are built.
    pub fn finish(mut self) -> DataDiff {
        let d = Dialect(self.kind);
        let t = d.qualified(&self.spec.table);
        let mut gone: Vec<Vec<Cell>> = std::mem::take(&mut self.target).into_keys().collect();
        gone.sort_by_key(|k| self.key_text(k));
        self.diff.deleted = gone.len() as u64;
        for k in gone.iter().take(SAMPLES) {
            self.sample(k, Status::OnlyTarget, Vec::new());
        }
        let delete = |cond: String| match self.kind {
            DbKind::ClickHouse => format!("ALTER TABLE {t} DELETE WHERE {cond}{CH_SYNC}"),
            _ => format!("DELETE FROM {t} WHERE {cond}"),
        };
        if let [k] = self.spec.key[..] {
            let col = &self.spec.columns[k];
            for chunk in gone.chunks(DELETE_BATCH) {
                let list = chunk.iter().map(|key| dml::literal(d, &key[0], col.numeric)).collect::<Vec<_>>().join(", ");
                self.diff.deletes.push(delete(format!("{} IN ({list})", d.ident(&col.column))));
            }
        } else {
            for key in &gone {
                let values: Vec<dml::Value<'_>> = self
                    .spec
                    .key
                    .iter()
                    .zip(key)
                    .map(|(&k, c)| dml::Value::new(&self.spec.columns[k].column, c, self.spec.columns[k].numeric))
                    .collect();
                self.diff.deletes.push(delete(dml::condition(d, &values)));
            }
        }
        if !self.inserts.is_empty() {
            let (before, after) = import::identity_statements(self.kind, &self.spec.table, &self.spec.columns);
            self.diff.inserts.extend(before);
            let names = self.spec.columns.iter().map(|c| d.ident(&c.column)).collect::<Vec<_>>().join(", ");
            for chunk in self.inserts.chunks(import::default_batch(self.kind)) {
                let mut sql = format!("INSERT INTO {t} ({names}) VALUES");
                for (i, row) in chunk.iter().enumerate() {
                    sql.push_str(if i == 0 { "\n(" } else { ",\n(" });
                    let values: Vec<String> =
                        row.iter().zip(&self.spec.columns).map(|(cell, c)| dml::literal(d, cell, c.numeric)).collect();
                    sql.push_str(&values.join(", "));
                    sql.push(')');
                }
                self.diff.inserts.push(sql);
            }
            self.diff.inserts.extend(after);
        }
        self.diff
    }
}

/// Orders tables so that referenced ones come first; `tables` are names with
/// the names they reference.
pub fn table_order(tables: &[(String, Vec<String>)]) -> Vec<usize> {
    let refs: Vec<Vec<usize>> = tables
        .iter()
        .map(|(_, r)| r.iter().filter_map(|n| tables.iter().position(|(t, _)| t == n)).collect())
        .collect();
    dependency_order(&refs)
}

/// The data script for several tables, given parents first: deletes run
/// children first, inserts parents first.
pub fn data_plan(diffs: &[&DataDiff]) -> Plan {
    let mut plan = Plan::default();
    for d in diffs.iter().rev() {
        plan.statements.extend(d.deletes.iter().cloned());
    }
    for d in diffs {
        plan.statements.extend(d.updates.iter().cloned());
    }
    for d in diffs {
        plan.statements.extend(d.inserts.iter().cloned());
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnInfo, ForeignKeyInfo, IndexInfo};

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, primary_key: pk, ..Default::default() }
    }

    fn table(schema: &str, name: &str, details: TableDetails) -> SchemaObject {
        SchemaObject {
            object: DbObject {
                kind: ObjectKind::Table,
                path: ObjectPath::new("db", schema, name),
                detail: None,
                comment: None,
            },
            details: Some(details),
            ddl: Ok(format!("CREATE TABLE {schema}.{name} (...)")),
        }
    }

    fn view(schema: &str, name: &str, ddl: &str) -> SchemaObject {
        SchemaObject {
            object: DbObject {
                kind: ObjectKind::View,
                path: ObjectPath::new("db", schema, name),
                detail: None,
                comment: None,
            },
            details: None,
            ddl: Ok(ddl.into()),
        }
    }

    fn snap(schema: &str, objects: Vec<SchemaObject>) -> Snapshot {
        Snapshot { database: "db".into(), schema: schema.into(), objects }
    }

    fn customers(extra: bool) -> TableDetails {
        let mut columns = vec![col("id", "integer", true), col("name", "text", false)];
        if extra {
            columns.push(col("email", "varchar(100)", false));
        }
        TableDetails { columns, ..Default::default() }
    }

    fn orders(schema: &str) -> TableDetails {
        TableDetails {
            columns: vec![col("id", "integer", true), col("customer_id", "integer", false)],
            indexes: vec![IndexInfo {
                name: "orders_customer".into(),
                columns: vec!["customer_id".into()],
                ..Default::default()
            }],
            foreign_keys: vec![ForeignKeyInfo {
                name: "orders_customer_fk".into(),
                columns: vec!["customer_id".into()],
                ref_schema: schema.into(),
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            comment: None,
        }
    }

    #[test]
    fn schema_diff_and_script() {
        let source = snap(
            "dev",
            vec![
                table("dev", "customers", customers(true)),
                table("dev", "orders", orders("dev")),
                view("dev", "big", "CREATE OR REPLACE VIEW dev.big AS\nSELECT * FROM dev.orders"),
                view("dev", "same", "CREATE VIEW \"dev\".\"same\" AS SELECT 1"),
            ],
        );
        let target = snap(
            "prod",
            vec![
                table("prod", "customers", customers(false)),
                table("prod", "legacy", customers(false)),
                view("prod", "same", "CREATE VIEW \"prod\".\"same\" AS  SELECT 1;"),
            ],
        );
        let diffs = compare(DbKind::Postgres, &source, &target);
        let get = |n: &str| diffs.iter().find(|d| d.name == n).unwrap();
        assert_eq!(get("customers").status, Status::Changed);
        assert_eq!(get("customers").changes, vec!["Add column email varchar(100)"]);
        assert_eq!(get("customers").apply, vec![r#"ALTER TABLE "prod"."customers" ADD COLUMN "email" varchar(100)"#]);
        assert_eq!(get("orders").status, Status::OnlySource);
        assert!(get("orders").apply[0].contains(r#"REFERENCES "prod"."customers""#), "{:?}", get("orders").apply);
        assert_eq!(get("legacy").status, Status::OnlyTarget);
        assert_eq!(get("same").status, Status::Same);
        assert_eq!(get("big").apply, vec!["CREATE OR REPLACE VIEW prod.big AS\nSELECT * FROM prod.orders"]);

        let chosen: Vec<&ObjectDiff> = diffs.iter().filter(|d| d.status != Status::Same).collect();
        let plan = sync_plan(&chosen);
        let first = |needle: &str| plan.statements.iter().position(|s| s.contains(needle)).unwrap();
        assert!(first("DROP TABLE") < first("CREATE TABLE"));
        assert!(first("CREATE TABLE \"prod\".\"orders\"") < first("CREATE OR REPLACE VIEW prod.big"));
        assert!(script(DbKind::MsSql, &plan).contains("\nGO\n"));
    }

    #[test]
    fn new_tables_follow_their_references() {
        let source = snap("s", vec![table("s", "a_orders", orders("s")), table("s", "customers", customers(false))]);
        let diffs = compare(DbKind::Postgres, &source, &snap("s", vec![]));
        let plan = sync_plan(&diffs.iter().collect::<Vec<_>>());
        assert!(plan.statements[0].contains("\"customers\""), "{:?}", plan.statements);
    }

    #[test]
    fn qualifiers_are_replaced_as_whole_words() {
        let d = Dialect(DbKind::Postgres);
        assert_eq!(
            replace_qualifier("SELECT dev.a, xdev.b, \"dev\".c, t.dev.d FROM dev.t", d, "dev", "prod"),
            "SELECT prod.a, xdev.b, \"prod\".c, t.dev.d FROM prod.t"
        );
    }

    fn row(cells: &[&str]) -> Vec<Cell> {
        cells.iter().map(|c| if *c == "NULL" { Cell::Null } else { Cell::Text(c.to_string()) }).collect()
    }

    #[test]
    fn data_diff_by_key() {
        let details = TableDetails {
            columns: vec![col("id", "integer", true), col("name", "text", false), col("n", "integer", false)],
            ..Default::default()
        };
        let path = ObjectPath::new("db", "public", "t");
        let spec = DataSpec::new(path, &details, &details).unwrap();
        let mut differ = DataDiffer::new(DbKind::Postgres, spec);
        for r in [["1", "a", "1"], ["2", "b", "2"], ["3", "c", "3"], ["4", "d", "NULL"]] {
            differ.add_target(row(&r));
        }
        for r in [["1", "a", "1"], ["2", "B", "2"], ["4", "d", "4"], ["5", "e", "5"]] {
            differ.add_source(row(&r));
        }
        let diff = differ.finish();
        assert_eq!((diff.source_rows, diff.target_rows), (4, 4));
        assert_eq!((diff.same, diff.updated, diff.inserted, diff.deleted), (1, 2, 1, 1));
        assert_eq!(diff.deletes, vec![r#"DELETE FROM "public"."t" WHERE "id" IN (3)"#]);
        assert_eq!(
            diff.updates,
            vec![
                r#"UPDATE "public"."t" SET "name" = 'B' WHERE "id" = 2"#,
                r#"UPDATE "public"."t" SET "n" = 4 WHERE "id" = 4"#
            ]
        );
        assert_eq!(diff.inserts, vec!["INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"n\") VALUES\n(5, 'e', 5)"]);
        assert_eq!(diff.samples.iter().find(|s| s.status == Status::Changed).unwrap().columns, vec!["name"]);

        let mut other = details.clone();
        other.columns[0].primary_key = false;
        assert!(DataSpec::new(ObjectPath::default(), &details, &other).is_err());
    }

    #[test]
    fn clickhouse_mutations() {
        let details =
            TableDetails { columns: vec![col("id", "UInt32", true), col("v", "String", false)], ..Default::default() };
        let spec = DataSpec::new(ObjectPath::new("db", "db", "t"), &details, &details).unwrap();
        let mut differ = DataDiffer::new(DbKind::ClickHouse, spec);
        differ.add_target(row(&["1", "x"]));
        differ.add_target(row(&["2", "y"]));
        differ.add_source(row(&["1", "z"]));
        let diff = differ.finish();
        assert_eq!(
            diff.updates,
            vec!["ALTER TABLE `db`.`t` UPDATE `v` = 'z' WHERE `id` = 1 SETTINGS mutations_sync = 2"]
        );
        assert_eq!(diff.deletes, vec!["ALTER TABLE `db`.`t` DELETE WHERE `id` IN (2) SETTINGS mutations_sync = 2"]);
    }
}
