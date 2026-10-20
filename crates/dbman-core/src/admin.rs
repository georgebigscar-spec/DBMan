//! Users, roles and privileges: the statements that list them on each engine,
//! that tell whether the connected user may manage them, and that create,
//! change and drop them.
//!
//! PostgreSQL roles are server-wide, and their privileges are listed for one
//! database. MySQL and MariaDB accounts are `user@host`. On SQL Server the
//! users and roles are those of one database, and a new user gets a login of
//! the same name. ClickHouse has users and roles of its SQL access control.

use crate::sql::Dialect;
use crate::{Cell, DbKind, QueryResult};

/// A user or a role (a group of users).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Principal {
    pub name: String,
    /// A role (group) rather than a user that logs in.
    pub role: bool,
    /// MySQL: the account's host (`%`, `localhost`); empty for MariaDB roles.
    pub host: String,
    /// SQL Server: the login of a database user.
    pub login: String,
    /// Keys of the [`options`] that are on.
    pub options: Vec<String>,
    /// Anything else the engine tells, such as the authentication plugin.
    pub note: String,
    /// Built in (`pg_*`, `mysql.sys`, `dbo`, fixed database roles).
    pub system: bool,
}

impl Principal {
    /// How memberships name it: the name, and on MySQL `user@host`.
    pub fn id(&self) -> String {
        if self.host.is_empty() { self.name.clone() } else { format!("{}@{}", self.name, self.host) }
    }

    pub fn has(&self, option: &str) -> bool {
        self.options.iter().any(|o| o == option)
    }
}

/// A switch on a user or role, such as `SUPERUSER` or a locked account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleOption {
    pub key: &'static str,
    pub label: &'static str,
    /// What a new user or role gets when the statement does not say.
    pub default: bool,
    /// Only users have it (it belongs to the login or the account).
    pub users_only: bool,
}

const fn opt(key: &'static str, label: &'static str, default: bool, users_only: bool) -> RoleOption {
    RoleOption { key, label, default, users_only }
}

/// The switches a user or role has on this engine.
pub fn options(kind: DbKind) -> &'static [RoleOption] {
    const PG: &[RoleOption] = &[
        opt("LOGIN", "Can log in", false, false),
        opt("SUPERUSER", "Superuser", false, false),
        opt("CREATEDB", "Can create databases", false, false),
        opt("CREATEROLE", "Can create roles", false, false),
        opt("INHERIT", "Inherits privileges of its roles", true, false),
        opt("REPLICATION", "Replication", false, false),
        opt("BYPASSRLS", "Bypasses row-level security", false, false),
    ];
    const MYSQL: &[RoleOption] = &[opt("LOCKED", "Account locked", false, true)];
    const MSSQL: &[RoleOption] = &[opt("DISABLED", "Login disabled", false, true)];
    match kind {
        DbKind::Postgres => PG,
        DbKind::MySql => MYSQL,
        DbKind::MsSql => MSSQL,
        _ => &[],
    }
}

/// Engines with users and roles to manage.
pub fn supported(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::MySql | DbKind::MsSql | DbKind::ClickHouse)
}

/// Whether privileges belong to one database, so the view picks which.
pub fn per_database(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::MsSql)
}

/// One row whose first value is true when the connected user may create,
/// change and drop users and roles. An error means it may not.
pub fn check_sql(kind: DbKind) -> Option<&'static str> {
    Some(match kind {
        DbKind::Postgres => "SELECT rolsuper OR rolcreaterole FROM pg_roles WHERE rolname = current_user",
        DbKind::MySql => {
            "SELECT COUNT(*) FROM information_schema.USER_PRIVILEGES WHERE PRIVILEGE_TYPE = 'CREATE USER' \
             AND GRANTEE = CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', \
             SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')"
        }
        DbKind::MsSql => {
            "SELECT CASE WHEN IS_SRVROLEMEMBER('sysadmin') = 1 OR (HAS_PERMS_BY_NAME(NULL, NULL, 'ALTER ANY LOGIN') = 1 \
             AND HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'ALTER ANY USER') = 1) THEN 1 ELSE 0 END"
        }
        // Reading the users takes the access management rights.
        DbKind::ClickHouse => "SELECT count() FROM system.users",
        DbKind::Sqlite | DbKind::DuckDb => return None,
    })
}

/// Reads the answer of [`check_sql`].
pub fn parse_check(result: &QueryResult) -> bool {
    result.rows.first().and_then(|r| r.first()).is_some_and(|c| truthy(&text(Some(c))))
}

/// Statements that list users and roles, tried in order until one works, with
/// the columns name, role, options (comma-separated keys), host, login, note
/// and system.
pub fn principals_sql(kind: DbKind) -> &'static [&'static str] {
    match kind {
        DbKind::Postgres => &["SELECT rolname, NOT rolcanlogin, concat_ws(',', \
             CASE WHEN rolcanlogin THEN 'LOGIN' END, CASE WHEN rolsuper THEN 'SUPERUSER' END, \
             CASE WHEN rolcreatedb THEN 'CREATEDB' END, CASE WHEN rolcreaterole THEN 'CREATEROLE' END, \
             CASE WHEN rolinherit THEN 'INHERIT' END, CASE WHEN rolreplication THEN 'REPLICATION' END, \
             CASE WHEN rolbypassrls THEN 'BYPASSRLS' END), '', '', \
             concat_ws(', ', CASE WHEN rolconnlimit >= 0 THEN 'connection limit ' || rolconnlimit END, \
             CASE WHEN rolvaliduntil IS NOT NULL THEN 'valid until ' || rolvaliduntil END), \
             rolname LIKE 'pg\\_%' FROM pg_roles ORDER BY rolname"],
        // MariaDB marks roles; MySQL roles are locked accounts without a password.
        DbKind::MySql => &[
            "SELECT User, is_role, '', Host, '', plugin, User IN ('mariadb.sys') \
             FROM mysql.user ORDER BY User, Host",
            "SELECT User, account_locked = 'Y' AND authentication_string = '', \
             CASE WHEN account_locked = 'Y' AND authentication_string <> '' THEN 'LOCKED' ELSE '' END, Host, '', plugin, \
             User IN ('mysql.sys', 'mysql.session', 'mysql.infoschema') FROM mysql.user ORDER BY User, Host",
        ],
        DbKind::MsSql => &["SELECT dp.name, CASE WHEN dp.type = 'R' THEN 1 ELSE 0 END, \
             CASE WHEN sp.is_disabled = 1 THEN 'DISABLED' ELSE '' END, '', ISNULL(sp.name, ''), \
             CONCAT(LOWER(dp.type_desc), CASE WHEN dp.default_schema_name IS NULL THEN '' \
             ELSE ', schema ' + dp.default_schema_name END), \
             CASE WHEN dp.is_fixed_role = 1 OR dp.name IN ('dbo', 'guest', 'INFORMATION_SCHEMA', 'sys', 'public') \
             THEN 1 ELSE 0 END \
             FROM sys.database_principals dp \
             LEFT JOIN sys.server_principals sp ON sp.sid = dp.sid AND dp.type <> 'R' \
             WHERE dp.type IN ('S', 'U', 'G', 'R', 'E', 'X') ORDER BY dp.name"],
        DbKind::ClickHouse => &["SELECT * FROM (\
             SELECT name, '0', '', '', '', concat('storage ', storage), '0' FROM system.users \
             UNION ALL SELECT name, '1', '', '', '', concat('storage ', storage), '0' FROM system.roles) \
             ORDER BY 1"],
        DbKind::Sqlite | DbKind::DuckDb => &[],
    }
}

/// Reads the rows of [`principals_sql`].
pub fn parse_principals(result: &QueryResult) -> Vec<Principal> {
    result
        .rows
        .iter()
        .map(|r| Principal {
            name: text(r.first()),
            role: truthy(&text(r.get(1))),
            options: text(r.get(2)).split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            host: text(r.get(3)),
            login: text(r.get(4)),
            note: text(r.get(5)),
            system: truthy(&text(r.get(6))),
        })
        .collect()
}

/// A role granted to a user or another role.
#[derive(Debug, Clone, PartialEq)]
pub struct Membership {
    /// [`Principal::id`] of the role.
    pub role: String,
    /// [`Principal::id`] of the member.
    pub member: String,
    /// The member may grant the role to others.
    pub admin: bool,
}

/// Statements that list role memberships, tried in order, with the columns
/// role, member and admin option.
pub fn memberships_sql(kind: DbKind) -> &'static [&'static str] {
    match kind {
        DbKind::Postgres => &["SELECT r.rolname, m.rolname, am.admin_option FROM pg_auth_members am \
             JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member"],
        DbKind::MySql => &[
            "SELECT Role, CONCAT(User, IF(Host = '', '', CONCAT('@', Host))), Admin_option FROM mysql.roles_mapping",
            "SELECT CONCAT(FROM_USER, IF(FROM_HOST = '', '', CONCAT('@', FROM_HOST))), \
             CONCAT(TO_USER, IF(TO_HOST = '', '', CONCAT('@', TO_HOST))), WITH_ADMIN_OPTION FROM mysql.role_edges",
        ],
        DbKind::MsSql => &["SELECT r.name, m.name, 0 FROM sys.database_role_members rm \
             JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id \
             JOIN sys.database_principals m ON m.principal_id = rm.member_principal_id"],
        DbKind::ClickHouse => &["SELECT granted_role_name, ifNull(user_name, role_name), with_admin_option \
             FROM system.role_grants"],
        DbKind::Sqlite | DbKind::DuckDb => &[],
    }
}

/// Reads the rows of [`memberships_sql`].
pub fn parse_memberships(result: &QueryResult) -> Vec<Membership> {
    result
        .rows
        .iter()
        .map(|r| Membership { role: text(r.first()), member: text(r.get(1)), admin: truthy(&text(r.get(2))) })
        .collect()
}

/// What a privilege is granted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything on the server (`*.*`).
    Server,
    Database,
    Schema,
    /// A table or view (an object of any kind on SQL Server).
    Table,
    Sequence,
    Function,
    /// Every table of a schema, now (PostgreSQL `ALL TABLES IN SCHEMA`).
    AllTablesInSchema,
}

impl Scope {
    pub fn label(self, kind: DbKind) -> &'static str {
        match self {
            Scope::Server => "Server",
            Scope::Database => "Database",
            Scope::Schema => "Schema",
            Scope::Table if kind == DbKind::MsSql => "Object",
            Scope::Table => "Table",
            Scope::Sequence => "Sequence",
            Scope::Function => "Function",
            Scope::AllTablesInSchema => "All tables in schema",
        }
    }

    /// Which of database, schema and name the target needs.
    pub fn fields(self, kind: DbKind) -> (bool, bool, bool) {
        let mysql_like = matches!(kind, DbKind::MySql | DbKind::ClickHouse);
        match self {
            Scope::Server => (false, false, false),
            Scope::Database => (kind != DbKind::MsSql, false, false),
            Scope::Schema | Scope::AllTablesInSchema => (false, true, false),
            Scope::Table if mysql_like => (true, false, true),
            Scope::Table | Scope::Sequence | Scope::Function => (false, true, true),
        }
    }
}

/// The scopes privileges can be granted on.
pub fn scopes(kind: DbKind) -> &'static [Scope] {
    match kind {
        DbKind::Postgres => {
            &[Scope::Table, Scope::AllTablesInSchema, Scope::Schema, Scope::Database, Scope::Sequence, Scope::Function]
        }
        DbKind::MySql | DbKind::ClickHouse => &[Scope::Table, Scope::Database, Scope::Server],
        DbKind::MsSql => &[Scope::Table, Scope::Schema, Scope::Database],
        DbKind::Sqlite | DbKind::DuckDb => &[],
    }
}

/// The privileges that can be granted on a scope.
pub fn privileges(kind: DbKind, scope: Scope) -> &'static [&'static str] {
    const PG_TABLE: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];
    const MY_TABLE: &[&str] = &[
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "CREATE",
        "DROP",
        "ALTER",
        "INDEX",
        "REFERENCES",
        "TRIGGER",
        "SHOW VIEW",
    ];
    const MY_DB: &[&str] = &[
        "ALL PRIVILEGES",
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "CREATE",
        "DROP",
        "ALTER",
        "INDEX",
        "REFERENCES",
        "CREATE VIEW",
        "SHOW VIEW",
        "CREATE ROUTINE",
        "ALTER ROUTINE",
        "EXECUTE",
        "EVENT",
        "TRIGGER",
        "LOCK TABLES",
        "CREATE TEMPORARY TABLES",
    ];
    const MY_SERVER: &[&str] = &[
        "ALL PRIVILEGES",
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "CREATE",
        "DROP",
        "ALTER",
        "INDEX",
        "CREATE VIEW",
        "SHOW VIEW",
        "CREATE ROUTINE",
        "ALTER ROUTINE",
        "EXECUTE",
        "EVENT",
        "TRIGGER",
        "PROCESS",
        "RELOAD",
        "SHOW DATABASES",
        "CREATE USER",
        "REPLICATION CLIENT",
        "REPLICATION SLAVE",
        "FILE",
        "SHUTDOWN",
    ];
    const MS_OBJECT: &[&str] =
        &["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "ALTER", "VIEW DEFINITION", "CONTROL"];
    const MS_DB: &[&str] = &[
        "CONNECT",
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "EXECUTE",
        "CREATE TABLE",
        "CREATE VIEW",
        "CREATE PROCEDURE",
        "CREATE FUNCTION",
        "CREATE SCHEMA",
        "VIEW DEFINITION",
        "SHOWPLAN",
        "BACKUP DATABASE",
        "ALTER",
        "CONTROL",
    ];
    const CH_TABLE: &[&str] = &["SELECT", "INSERT", "ALTER", "TRUNCATE", "OPTIMIZE", "DROP TABLE", "SHOW TABLES"];
    const CH_DB: &[&str] = &[
        "ALL",
        "SELECT",
        "INSERT",
        "ALTER",
        "CREATE TABLE",
        "CREATE VIEW",
        "DROP TABLE",
        "TRUNCATE",
        "OPTIMIZE",
        "SHOW TABLES",
    ];
    const CH_SERVER: &[&str] = &[
        "ALL",
        "SELECT",
        "INSERT",
        "ALTER",
        "CREATE",
        "DROP",
        "TRUNCATE",
        "OPTIMIZE",
        "SHOW",
        "KILL QUERY",
        "SYSTEM",
        "ACCESS MANAGEMENT",
    ];
    match (kind, scope) {
        (DbKind::Postgres, Scope::Table | Scope::AllTablesInSchema) => PG_TABLE,
        (DbKind::Postgres, Scope::Schema) => &["USAGE", "CREATE"],
        (DbKind::Postgres, Scope::Database) => &["CONNECT", "CREATE", "TEMPORARY"],
        (DbKind::Postgres, Scope::Sequence) => &["USAGE", "SELECT", "UPDATE"],
        (DbKind::Postgres, Scope::Function) => &["EXECUTE"],
        (DbKind::MySql, Scope::Table) => MY_TABLE,
        (DbKind::MySql, Scope::Database) => MY_DB,
        (DbKind::MySql, Scope::Server) => MY_SERVER,
        (DbKind::MsSql, Scope::Table | Scope::Schema) => MS_OBJECT,
        (DbKind::MsSql, Scope::Database) => MS_DB,
        (DbKind::ClickHouse, Scope::Table) => CH_TABLE,
        (DbKind::ClickHouse, Scope::Database) => CH_DB,
        (DbKind::ClickHouse, Scope::Server) => CH_SERVER,
        _ => &[],
    }
}

/// What to grant privileges on: the parts [`Scope::fields`] asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub scope: Scope,
    pub database: String,
    pub schema: String,
    pub name: String,
}

/// The text after `ON` in a `GRANT` (empty for SQL Server database privileges,
/// which have no `ON`).
pub fn target_sql(kind: DbKind, t: &Target) -> String {
    let q = |s: &str| Dialect(kind).ident(s);
    let qualified = |a: &str, b: &str| format!("{}.{}", q(a), q(b));
    match (kind, t.scope) {
        (DbKind::Postgres, Scope::Database) => format!("DATABASE {}", q(&t.database)),
        (DbKind::Postgres, Scope::Schema) => format!("SCHEMA {}", q(&t.schema)),
        (DbKind::Postgres, Scope::AllTablesInSchema) => format!("ALL TABLES IN SCHEMA {}", q(&t.schema)),
        (DbKind::Postgres, Scope::Table) => format!("TABLE {}", qualified(&t.schema, &t.name)),
        (DbKind::Postgres, Scope::Sequence) => format!("SEQUENCE {}", qualified(&t.schema, &t.name)),
        (DbKind::Postgres, Scope::Function) => format!("FUNCTION {}", qualified(&t.schema, &t.name)),
        (DbKind::MsSql, Scope::Database) => String::new(),
        (DbKind::MsSql, Scope::Schema) => format!("SCHEMA::{}", q(&t.schema)),
        (DbKind::MsSql, _) => format!("OBJECT::{}", qualified(&t.schema, &t.name)),
        (_, Scope::Server) => "*.*".into(),
        (_, Scope::Database) => format!("{}.*", q(&t.database)),
        _ => qualified(&t.database, &t.name),
    }
}

/// A privilege a user or role holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    /// The kind of object: `Table`, `View`, `Schema`, `Database`, `Server`…
    pub scope: String,
    /// The object as people read it.
    pub object: String,
    /// The object as [`target_sql`] writes it, for the `REVOKE`.
    pub target: String,
    pub privilege: String,
    /// Granted `WITH GRANT OPTION`.
    pub grantable: bool,
}

/// The statement that lists the privileges granted straight to `p` (not
/// through its roles), with the columns scope, object, target, privilege and
/// grantable. On PostgreSQL and SQL Server it lists those of the database it
/// runs in.
pub fn grants_sql(kind: DbKind, p: &Principal) -> Option<String> {
    let s = |v: &str| Dialect(kind).string(v);
    Some(match kind {
        DbKind::Postgres => format!(
            "WITH acl AS (\
             SELECT 'Database' AS scope, quote_ident(datname) AS object, 'DATABASE ' || quote_ident(datname) AS target, \
             datacl AS acl FROM pg_database \
             UNION ALL SELECT 'Schema', quote_ident(nspname), 'SCHEMA ' || quote_ident(nspname), nspacl \
             FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
             UNION ALL SELECT CASE c.relkind WHEN 'S' THEN 'Sequence' WHEN 'v' THEN 'View' \
             WHEN 'm' THEN 'Materialized view' ELSE 'Table' END, \
             quote_ident(n.nspname) || '.' || quote_ident(c.relname), \
             CASE c.relkind WHEN 'S' THEN 'SEQUENCE ' ELSE 'TABLE ' END || quote_ident(n.nspname) || '.' || quote_ident(c.relname), \
             c.relacl FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
             UNION ALL SELECT CASE p.prokind WHEN 'p' THEN 'Procedure' ELSE 'Function' END, p.oid::regprocedure::text, \
             CASE p.prokind WHEN 'p' THEN 'PROCEDURE ' ELSE 'FUNCTION ' END || p.oid::regprocedure::text, p.proacl \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema') \
             SELECT scope, object, target, a.privilege_type, a.is_grantable \
             FROM acl, aclexplode(acl.acl) a \
             WHERE a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {}) \
             ORDER BY 1, 2, 4",
            s(&p.name)
        ),
        DbKind::MySql => {
            let g = s(&account(kind, p));
            let id = |col: &str| format!("CONCAT('`', REPLACE({col}, '`', '``'), '`')");
            format!(
                "SELECT 'Server', '*.*', '*.*', PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES \
                 WHERE GRANTEE = {g} AND PRIVILEGE_TYPE <> 'USAGE' \
                 UNION ALL SELECT 'Database', TABLE_SCHEMA, CONCAT({db}, '.*'), PRIVILEGE_TYPE, IS_GRANTABLE \
                 FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE = {g} \
                 UNION ALL SELECT 'Table', CONCAT(TABLE_SCHEMA, '.', TABLE_NAME), CONCAT({db}, '.', {table}), \
                 PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = {g} \
                 ORDER BY 1, 2, 4",
                db = id("TABLE_SCHEMA"),
                table = id("TABLE_NAME"),
            )
        }
        DbKind::MsSql => format!(
            "SELECT CASE p.class WHEN 0 THEN 'Database' WHEN 3 THEN 'Schema' ELSE 'Object' END, \
             CASE p.class WHEN 0 THEN DB_NAME() WHEN 3 THEN s.name \
             ELSE OBJECT_SCHEMA_NAME(p.major_id) + '.' + OBJECT_NAME(p.major_id) END, \
             CASE p.class WHEN 0 THEN '' WHEN 3 THEN 'SCHEMA::' + QUOTENAME(s.name) \
             ELSE 'OBJECT::' + QUOTENAME(OBJECT_SCHEMA_NAME(p.major_id)) + '.' + QUOTENAME(OBJECT_NAME(p.major_id)) END, \
             p.permission_name, CASE WHEN p.state = 'W' THEN 1 ELSE 0 END \
             FROM sys.database_permissions p \
             JOIN sys.database_principals g ON g.principal_id = p.grantee_principal_id \
             LEFT JOIN sys.schemas s ON p.class = 3 AND s.schema_id = p.major_id \
             WHERE g.name = {} AND p.state IN ('G', 'W') AND p.class IN (0, 1, 3) AND p.minor_id = 0 \
             ORDER BY 1, 2, 4",
            s(&p.name)
        ),
        DbKind::ClickHouse => {
            let id = |col: &str| format!("if({col} IS NULL, '*', concat('`', replaceAll({col}, '`', '\\\\`'), '`'))");
            format!(
                "SELECT multiIf(database IS NULL, 'Server', table IS NULL, 'Database', 'Table'), \
                 concat(ifNull(database, '*'), '.', ifNull(table, '*')), concat({db}, '.', {table}), \
                 toString(access_type), grant_option FROM system.grants \
                 WHERE (user_name = {name} OR role_name = {name}) AND NOT is_partial_revoke AND column IS NULL \
                 ORDER BY 1, 2, 4",
                db = id("database"),
                table = id("table"),
                name = s(&p.name),
            )
        }
        DbKind::Sqlite | DbKind::DuckDb => return None,
    })
}

/// Reads the rows of [`grants_sql`].
pub fn parse_grants(result: &QueryResult) -> Vec<Grant> {
    result
        .rows
        .iter()
        .map(|r| Grant {
            scope: text(r.first()),
            object: text(r.get(1)),
            target: text(r.get(2)),
            privilege: text(r.get(3)),
            grantable: truthy(&text(r.get(4))),
        })
        .collect()
}

/// A user or role as statements name it: `"name"`, `'user'@'host'`, `[name]`.
pub fn account(kind: DbKind, p: &Principal) -> String {
    let d = Dialect(kind);
    match kind {
        DbKind::MySql if p.host.is_empty() => d.string(&p.name),
        DbKind::MySql => format!("{}@{}", d.string(&p.name), d.string(&p.host)),
        _ => d.ident(&p.name),
    }
}

/// What the editor of a user or role asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edit {
    pub name: String,
    /// MySQL only.
    pub host: String,
    pub role: bool,
    /// Left empty to keep (or, for a new user, not to set) the password.
    pub password: String,
    /// Keys of the options that should be on.
    pub options: Vec<String>,
}

impl Edit {
    /// The editor filled in for an existing user or role.
    pub fn of(p: &Principal) -> Self {
        Self {
            name: p.name.clone(),
            host: p.host.clone(),
            role: p.role,
            password: String::new(),
            options: p.options.clone(),
        }
    }

    /// A new user (or role) with the engine's defaults.
    pub fn new(kind: DbKind, role: bool) -> Self {
        let mut options: Vec<String> = options(kind).iter().filter(|o| o.default).map(|o| o.key.to_string()).collect();
        if kind == DbKind::Postgres && !role {
            options.insert(0, "LOGIN".into());
        }
        let host = if kind == DbKind::MySql && !role { "%".into() } else { String::new() };
        Self { name: String::new(), host, role, password: String::new(), options }
    }

    fn has(&self, key: &str) -> bool {
        self.options.iter().any(|o| o == key)
    }

    fn principal(&self) -> Principal {
        Principal {
            name: self.name.trim().into(),
            host: self.host.trim().into(),
            role: self.role,
            ..Default::default()
        }
    }
}

/// The statements that create a user or role.
pub fn create(kind: DbKind, e: &Edit) -> Vec<String> {
    let d = Dialect(kind);
    let p = e.principal();
    let acct = account(kind, &p);
    let what = if e.role { "ROLE" } else { "USER" };
    let password = (!e.password.is_empty() && !e.role).then(|| d.string(&e.password));
    match kind {
        DbKind::Postgres => {
            let mut words: Vec<String> = options(kind)
                .iter()
                .filter(|o| e.has(o.key) != o.default)
                .map(|o| if e.has(o.key) { o.key.to_string() } else { format!("NO{}", o.key) })
                .collect();
            if let Some(pw) = &password {
                words.push(format!("PASSWORD {pw}"));
            }
            if words.is_empty() {
                vec![format!("CREATE ROLE {acct}")]
            } else {
                vec![format!("CREATE ROLE {acct} WITH {}", words.join(" "))]
            }
        }
        DbKind::MySql => {
            let mut sql = format!("CREATE {what} {acct}");
            if !e.role {
                if let Some(pw) = &password {
                    sql.push_str(&format!(" IDENTIFIED BY {pw}"));
                }
                if e.has("LOCKED") {
                    sql.push_str(" ACCOUNT LOCK");
                }
            }
            vec![sql]
        }
        DbKind::MsSql if e.role => vec![format!("CREATE ROLE {acct}")],
        DbKind::MsSql => {
            let pw = password.unwrap_or_else(|| d.string(""));
            let mut out = vec![
                format!("CREATE LOGIN {acct} WITH PASSWORD = {pw}"),
                format!("CREATE USER {acct} FOR LOGIN {acct}"),
            ];
            if e.has("DISABLED") {
                out.push(format!("ALTER LOGIN {acct} DISABLE"));
            }
            out
        }
        DbKind::ClickHouse => match &password {
            Some(pw) => vec![format!("CREATE {what} {acct} IDENTIFIED BY {pw}")],
            None => vec![format!("CREATE {what} {acct}")],
        },
        DbKind::Sqlite | DbKind::DuckDb => Vec::new(),
    }
}

/// The statements that change `old` into what `e` says: options, password, then name.
pub fn alter(kind: DbKind, old: &Principal, e: &Edit) -> Vec<String> {
    let d = Dialect(kind);
    let acct = account(kind, old);
    let new = e.principal();
    let mut out = Vec::new();
    let changed: Vec<&RoleOption> =
        options(kind).iter().filter(|o| !(o.users_only && old.role) && old.has(o.key) != e.has(o.key)).collect();
    let password = (!e.password.is_empty() && !old.role).then(|| d.string(&e.password));
    match kind {
        DbKind::Postgres => {
            let mut words: Vec<String> =
                changed.iter().map(|o| if e.has(o.key) { o.key.to_string() } else { format!("NO{}", o.key) }).collect();
            if let Some(pw) = &password {
                words.push(format!("PASSWORD {pw}"));
            }
            if !words.is_empty() {
                out.push(format!("ALTER ROLE {acct} WITH {}", words.join(" ")));
            }
            if new.name != old.name {
                out.push(format!("ALTER ROLE {acct} RENAME TO {}", account(kind, &new)));
            }
        }
        DbKind::MySql => {
            if let Some(pw) = &password {
                out.push(format!("ALTER USER {acct} IDENTIFIED BY {pw}"));
            }
            if !changed.is_empty() {
                let lock = if e.has("LOCKED") { "LOCK" } else { "UNLOCK" };
                out.push(format!("ALTER USER {acct} ACCOUNT {lock}"));
            }
            if (new.name != old.name || new.host != old.host) && !old.role {
                out.push(format!("RENAME USER {acct} TO {}", account(kind, &new)));
            }
        }
        DbKind::MsSql => {
            let login = (!old.login.is_empty()).then(|| d.ident(&old.login));
            if let (Some(pw), Some(login)) = (&password, &login) {
                out.push(format!("ALTER LOGIN {login} WITH PASSWORD = {pw}"));
            }
            if let (false, Some(login)) = (changed.is_empty(), &login) {
                let state = if e.has("DISABLED") { "DISABLE" } else { "ENABLE" };
                out.push(format!("ALTER LOGIN {login} {state}"));
            }
            if new.name != old.name {
                let what = if old.role { "ROLE" } else { "USER" };
                out.push(format!("ALTER {what} {acct} WITH NAME = {}", account(kind, &new)));
            }
        }
        DbKind::ClickHouse => {
            let what = if old.role { "ROLE" } else { "USER" };
            if let Some(pw) = &password {
                out.push(format!("ALTER USER {acct} IDENTIFIED BY {pw}"));
            }
            if new.name != old.name {
                out.push(format!("ALTER {what} {acct} RENAME TO {}", account(kind, &new)));
            }
        }
        DbKind::Sqlite | DbKind::DuckDb => {}
    }
    out
}

/// The statement that drops a user or role. A SQL Server user's login stays.
pub fn drop(kind: DbKind, p: &Principal) -> String {
    let what = match kind {
        DbKind::Postgres => "ROLE",
        // MySQL roles are accounts, and DROP USER drops them too.
        DbKind::MySql if p.role && p.host.is_empty() => "ROLE",
        DbKind::MySql => "USER",
        _ if p.role => "ROLE",
        _ => "USER",
    };
    format!("DROP {what} {}", account(kind, p))
}

/// Makes `member` a member of `role`.
pub fn grant_role(kind: DbKind, role: &Principal, member: &Principal, admin: bool) -> String {
    let (r, m) = (account(kind, role), account(kind, member));
    if kind == DbKind::MsSql {
        return format!("ALTER ROLE {r} ADD MEMBER {m}");
    }
    let admin = if admin { " WITH ADMIN OPTION" } else { "" };
    format!("GRANT {r} TO {m}{admin}")
}

/// Takes `member` out of `role`.
pub fn revoke_role(kind: DbKind, role: &Principal, member: &Principal) -> String {
    let (r, m) = (account(kind, role), account(kind, member));
    if kind == DbKind::MsSql {
        return format!("ALTER ROLE {r} DROP MEMBER {m}");
    }
    format!("REVOKE {r} FROM {m}")
}

/// Grants privileges on a target (see [`target_sql`]).
pub fn grant(kind: DbKind, privileges: &[&str], target: &str, to: &Principal, grant_option: bool) -> String {
    let on = if target.is_empty() { String::new() } else { format!(" ON {target}") };
    let option = if grant_option { " WITH GRANT OPTION" } else { "" };
    format!("GRANT {}{on} TO {}{option}", privileges.join(", "), account(kind, to))
}

/// Takes privileges back. On SQL Server a privilege granted with the grant
/// option is also taken from those it was passed on to.
pub fn revoke(kind: DbKind, privileges: &[&str], target: &str, from: &Principal, grantable: bool) -> String {
    let on = if target.is_empty() { String::new() } else { format!(" ON {target}") };
    let cascade = if kind == DbKind::MsSql && grantable { " CASCADE" } else { "" };
    format!("REVOKE {}{on} FROM {}{cascade}", privileges.join(", "), account(kind, from))
}

fn text(c: Option<&Cell>) -> String {
    c.and_then(Cell::as_text).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// `t`, `true`, `Y`, `YES` and numbers other than 0.
fn truthy(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "t" | "true" | "y" | "yes") || s.parse::<f64>().is_ok_and(|n| n != 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    fn user(name: &str) -> Principal {
        Principal { name: name.into(), ..Default::default() }
    }

    #[test]
    fn creates_and_alters_postgres_roles() {
        let mut e = Edit::new(DbKind::Postgres, false);
        e.name = "app".into();
        e.password = "it's".into();
        e.options.push("CREATEDB".into());
        assert_eq!(create(DbKind::Postgres, &e), ["CREATE ROLE \"app\" WITH LOGIN CREATEDB PASSWORD 'it''s'"]);
        let mut role = Edit::new(DbKind::Postgres, true);
        role.name = "readers".into();
        assert_eq!(create(DbKind::Postgres, &role), ["CREATE ROLE \"readers\""]);

        let old =
            Principal { name: "app".into(), options: vec!["LOGIN".into(), "INHERIT".into()], ..Default::default() };
        let mut e = Edit::of(&old);
        e.options.retain(|o| o != "INHERIT");
        e.options.push("SUPERUSER".into());
        e.name = "app2".into();
        assert_eq!(
            alter(DbKind::Postgres, &old, &e),
            ["ALTER ROLE \"app\" WITH SUPERUSER NOINHERIT", "ALTER ROLE \"app\" RENAME TO \"app2\""]
        );
        assert!(alter(DbKind::Postgres, &old, &Edit::of(&old)).is_empty());
        assert_eq!(drop(DbKind::Postgres, &old), "DROP ROLE \"app\"");
    }

    #[test]
    fn mysql_accounts_have_hosts() {
        let mut e = Edit::new(DbKind::MySql, false);
        e.name = "app".into();
        e.password = "pw".into();
        e.options.push("LOCKED".into());
        assert_eq!(create(DbKind::MySql, &e), ["CREATE USER 'app'@'%' IDENTIFIED BY 'pw' ACCOUNT LOCK"]);
        let old = Principal { name: "app".into(), host: "%".into(), ..Default::default() };
        assert_eq!(old.id(), "app@%");
        let mut e = Edit::of(&old);
        e.host = "localhost".into();
        e.options.push("LOCKED".into());
        assert_eq!(
            alter(DbKind::MySql, &old, &e),
            ["ALTER USER 'app'@'%' ACCOUNT LOCK", "RENAME USER 'app'@'%' TO 'app'@'localhost'"]
        );
        let role = Principal { name: "r".into(), role: true, ..Default::default() };
        assert_eq!(drop(DbKind::MySql, &role), "DROP ROLE 'r'");
        assert_eq!(grant_role(DbKind::MySql, &role, &old, true), "GRANT 'r' TO 'app'@'%' WITH ADMIN OPTION");
        let t = Target { scope: Scope::Table, database: "shop".into(), schema: String::new(), name: "orders".into() };
        assert_eq!(
            grant(DbKind::MySql, &["SELECT", "INSERT"], &target_sql(DbKind::MySql, &t), &old, false),
            "GRANT SELECT, INSERT ON `shop`.`orders` TO 'app'@'%'"
        );
    }

    #[test]
    fn sql_server_users_get_logins() {
        let mut e = Edit::new(DbKind::MsSql, false);
        e.name = "app".into();
        e.password = "Pass_1".into();
        assert_eq!(
            create(DbKind::MsSql, &e),
            ["CREATE LOGIN [app] WITH PASSWORD = N'Pass_1'", "CREATE USER [app] FOR LOGIN [app]"]
        );
        let old = Principal { name: "app".into(), login: "app".into(), ..Default::default() };
        let mut e = Edit::of(&old);
        e.options.push("DISABLED".into());
        e.password = "x".into();
        assert_eq!(
            alter(DbKind::MsSql, &old, &e),
            ["ALTER LOGIN [app] WITH PASSWORD = N'x'", "ALTER LOGIN [app] DISABLE"]
        );
        let role = Principal { name: "readers".into(), role: true, ..Default::default() };
        assert_eq!(grant_role(DbKind::MsSql, &role, &old, false), "ALTER ROLE [readers] ADD MEMBER [app]");
        assert_eq!(revoke_role(DbKind::MsSql, &role, &old), "ALTER ROLE [readers] DROP MEMBER [app]");
        let db = Target { scope: Scope::Database, database: String::new(), schema: String::new(), name: String::new() };
        assert_eq!(
            grant(DbKind::MsSql, &["CREATE TABLE"], &target_sql(DbKind::MsSql, &db), &old, false),
            "GRANT CREATE TABLE TO [app]"
        );
        assert_eq!(
            revoke(DbKind::MsSql, &["SELECT"], "OBJECT::[dbo].[t]", &old, true),
            "REVOKE SELECT ON OBJECT::[dbo].[t] FROM [app] CASCADE"
        );
    }

    #[test]
    fn targets() {
        let t = |scope, db: &str, schema: &str, name: &str| Target {
            scope,
            database: db.into(),
            schema: schema.into(),
            name: name.into(),
        };
        let pg = DbKind::Postgres;
        assert_eq!(target_sql(pg, &t(Scope::Table, "", "public", "a\"b")), "TABLE \"public\".\"a\"\"b\"");
        assert_eq!(target_sql(pg, &t(Scope::AllTablesInSchema, "", "s", "")), "ALL TABLES IN SCHEMA \"s\"");
        assert_eq!(target_sql(pg, &t(Scope::Database, "shop", "", "")), "DATABASE \"shop\"");
        assert_eq!(target_sql(DbKind::ClickHouse, &t(Scope::Database, "db", "", "")), "`db`.*");
        assert_eq!(target_sql(DbKind::MySql, &t(Scope::Server, "", "", "")), "*.*");
        assert_eq!(target_sql(DbKind::MsSql, &t(Scope::Schema, "", "dbo", "")), "SCHEMA::[dbo]");
        assert_eq!(
            revoke(pg, &["SELECT"], "TABLE \"public\".\"t\"", &user("app"), true),
            "REVOKE SELECT ON TABLE \"public\".\"t\" FROM \"app\""
        );
        for kind in [DbKind::Postgres, DbKind::MySql, DbKind::MsSql, DbKind::ClickHouse] {
            for &scope in scopes(kind) {
                assert!(!privileges(kind, scope).is_empty(), "{kind:?} {scope:?}");
            }
            assert!(check_sql(kind).is_some() && !principals_sql(kind).is_empty());
        }
        assert!(!supported(DbKind::Sqlite) && scopes(DbKind::Sqlite).is_empty());
    }

    #[test]
    fn parses_rows() {
        let t = |s: &str| Cell::Text(s.into());
        let result = |rows: Vec<Vec<Cell>>| QueryResult {
            columns: (0..rows[0].len()).map(|i| ColumnMeta::new(format!("c{i}"), "text")).collect(),
            rows,
            truncated: false,
        };
        let p = parse_principals(&result(vec![vec![
            t("app"),
            t("f"),
            t("LOGIN,CREATEDB"),
            t(""),
            Cell::Null,
            t(""),
            t("false"),
        ]]));
        assert_eq!(p[0].options, ["LOGIN", "CREATEDB"]);
        assert!(!p[0].role && !p[0].system && p[0].has("CREATEDB"));
        assert!(parse_check(&result(vec![vec![t("t")]])));
        assert!(parse_check(&result(vec![vec![t("3")]])));
        assert!(!parse_check(&result(vec![vec![t("0")]])));
        let m = parse_memberships(&result(vec![vec![t("readers"), t("app@%"), t("YES")]]));
        assert_eq!(m[0], Membership { role: "readers".into(), member: "app@%".into(), admin: true });
    }
}
