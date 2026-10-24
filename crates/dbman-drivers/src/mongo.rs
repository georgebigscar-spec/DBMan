//! MongoDB. The explorer lists databases and their collections, a documents
//! tab browses and edits a collection, and a console runs mongo shell
//! statements (`db.users.find({age: {$gt: 30}})`).

use std::time::Duration;

use async_trait::async_trait;
use dbman_core::mongo::{self as m, Call, Reply, Shell};
use dbman_core::{
    ColumnInfo, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, IndexInfo, ObjectKind, ObjectPath,
    SslMode, StatementOutcome, TableDetails,
};
use futures_util::TryStreamExt;
use mongodb::bson::{Bson, Document, doc, oid::ObjectId};
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::results::CollectionType;
use mongodb::{Client, Database};
use serde_json::{Value, json};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Documents read to guess a collection's fields.
const SAMPLE: i64 = 200;

pub struct MongoConnection {
    cfg: ConnectionConfig,
    client: Client,
    /// The database statements run in when none is given.
    default_db: String,
}

fn err(e: impl std::fmt::Display) -> DbError {
    DbError::from_display(e)
}

/// Extended JSON as BSON; `ObjectId()` without a value becomes a new id.
pub fn to_bson(v: &Value) -> DbResult<Bson> {
    fn fresh(v: &Value) -> Value {
        match v {
            Value::Object(o) if o.len() == 1 && o.get("$oid").and_then(Value::as_str) == Some("new") => {
                json!({"$oid": ObjectId::new().to_hex()})
            }
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), fresh(v))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(fresh).collect()),
            other => other.clone(),
        }
    }
    Bson::try_from(fresh(v)).map_err(err)
}

fn to_doc(v: Option<&Value>) -> DbResult<Document> {
    match v {
        None | Some(Value::Null) => Ok(Document::new()),
        Some(v @ Value::Object(_)) => match to_bson(v)? {
            Bson::Document(d) => Ok(d),
            _ => Err(DbError::new("Expected a document.")),
        },
        Some(other) => Err(DbError::new(format!("Expected a document, got {other}."))),
    }
}

fn to_docs(v: Option<&Value>) -> DbResult<Vec<Document>> {
    match v {
        Some(Value::Array(items)) => items.iter().map(|i| to_doc(Some(i))).collect(),
        Some(v @ Value::Object(_)) => Ok(vec![to_doc(Some(v))?]),
        _ => Err(DbError::new("Expected an array of documents.")),
    }
}

fn json_of(d: Document) -> Value {
    Bson::Document(d).into_relaxed_extjson()
}

/// The name MongoDB gives an index of these keys: `name_1_age_-1`.
fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(n) => n.to_string(),
                other => other.to_string(),
            };
            format!("{k}_{v}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

impl MongoConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let host = cfg.host.trim();
        let mut options = if host.starts_with("mongodb://") || host.starts_with("mongodb+srv://") {
            ClientOptions::parse(host).await.map_err(err)?
        } else {
            let host = if host.is_empty() { "127.0.0.1" } else { host };
            let port = if cfg.port == 0 { 27017 } else { cfg.port };
            let mut o = ClientOptions::default();
            o.hosts = vec![ServerAddress::Tcp { host: host.to_string(), port: Some(port) }];
            // One server named by the user (or an SSH tunnel's end): talk to it, not to the
            // replica set members it would name.
            o.direct_connection = Some(true);
            o.tls = match cfg.ssl_mode {
                SslMode::Disable | SslMode::Prefer => None,
                SslMode::Require => Some(Tls::Enabled(TlsOptions::builder().allow_invalid_certificates(true).build())),
                SslMode::VerifyFull => Some(Tls::Enabled(TlsOptions::default())),
            };
            o
        };
        if !cfg.user.is_empty() {
            let source = options.credential.as_ref().and_then(|c| c.source.clone()).unwrap_or_else(|| "admin".into());
            options.credential = Some(
                Credential::builder().username(cfg.user.clone()).password(cfg.password.clone()).source(source).build(),
            );
        }
        options.app_name = Some("DBMan".into());
        options.connect_timeout = Some(CONNECT_TIMEOUT);
        options.server_selection_timeout = Some(CONNECT_TIMEOUT);
        let default_db = if !cfg.database.is_empty() {
            cfg.database.clone()
        } else {
            options.default_database.clone().unwrap_or_else(|| "test".into())
        };
        let client = Client::with_options(options).map_err(err)?;
        // Fail now rather than on the first statement.
        client.database("admin").run_command(doc! {"ping": 1}).await.map_err(err)?;
        Ok(Self { cfg: cfg.clone(), client, default_db })
    }

    fn db(&self, name: Option<&str>) -> Database {
        self.client.database(name.filter(|n| !n.is_empty()).unwrap_or(&self.default_db))
    }

    /// Runs a command that answers with a cursor, reading at most `max` documents.
    async fn cursor(&self, db: &Database, command: Document, max: usize) -> DbResult<Reply> {
        let mut cursor = db.run_cursor_command(command).await.map_err(err)?;
        let mut docs = Vec::new();
        while let Some(d) = cursor.try_next().await.map_err(err)? {
            if docs.len() == max {
                return Ok(Reply::Documents { docs, more: true });
            }
            docs.push(json_of(d));
        }
        Ok(Reply::Documents { docs, more: false })
    }

    async fn command(&self, db: &Database, command: Document) -> DbResult<Reply> {
        Ok(Reply::Value(json_of(db.run_command(command).await.map_err(err)?)))
    }

    async fn db_call(&self, db: &Database, call: &Call, max: usize) -> DbResult<Reply> {
        let arg = |i: usize| call.args.get(i);
        match call.method.as_str() {
            "runCommand" => self.command(db, to_doc(arg(0))?).await,
            "adminCommand" => self.command(&self.client.database("admin"), to_doc(arg(0))?).await,
            "getCollectionNames" => {
                let mut names = db.list_collection_names().await.map_err(err)?;
                names.sort();
                Ok(Reply::Value(json!(names)))
            }
            "getCollectionInfos" => {
                let mut cmd = doc! {"listCollections": 1};
                if let Some(f) = arg(0) {
                    cmd.insert("filter", to_doc(Some(f))?);
                }
                self.cursor(db, cmd, max).await
            }
            "stats" => self.command(db, doc! {"dbStats": 1}).await,
            "serverStatus" => self.command(db, doc! {"serverStatus": 1}).await,
            "hostInfo" => self.command(db, doc! {"hostInfo": 1}).await,
            "listCommands" => self.command(db, doc! {"listCommands": 1}).await,
            "version" => {
                let info = db.run_command(doc! {"buildInfo": 1}).await.map_err(err)?;
                Ok(Reply::Value(json!(info.get_str("version").unwrap_or_default())))
            }
            "getName" => Ok(Reply::Value(json!(db.name()))),
            "createCollection" => {
                let Some(Value::String(name)) = arg(0) else {
                    return Err(DbError::new("createCollection() expects a name."));
                };
                let mut cmd = doc! {"create": name};
                cmd.extend(to_doc(arg(1))?);
                db.run_command(cmd).await.map_err(err)?;
                Ok(Reply::Written { affected: 0 })
            }
            "createView" => {
                let (Some(Value::String(name)), Some(Value::String(on))) = (arg(0), arg(1)) else {
                    return Err(DbError::new("createView() expects a name, a source and a pipeline."));
                };
                let pipeline: Vec<Bson> = to_docs(arg(2))?.into_iter().map(Bson::Document).collect();
                db.run_command(doc! {"create": name, "viewOn": on, "pipeline": pipeline}).await.map_err(err)?;
                Ok(Reply::Written { affected: 0 })
            }
            "dropDatabase" => {
                db.drop().await.map_err(err)?;
                Ok(Reply::Written { affected: 0 })
            }
            other => {
                Err(DbError::new(format!("db.{other}() is not supported here; db.runCommand({{…}}) runs any command.")))
            }
        }
    }

    async fn collection_call(
        &self,
        db: &Database,
        name: &str,
        call: &Call,
        chain: &[Call],
        max: usize,
    ) -> DbResult<Reply> {
        let coll = db.collection::<Document>(name);
        let arg = |i: usize| call.args.get(i);
        let written = |n: u64| Ok(Reply::Written { affected: n });
        let options = |i: usize| -> DbResult<Document> { to_doc(arg(i)) };
        match call.method.as_str() {
            "find" | "findOne" => {
                let filter = to_doc(arg(0))?;
                if chain.iter().any(|c| matches!(c.method.as_str(), "count" | "itcount" | "size")) {
                    let n = coll.count_documents(filter).await.map_err(err)?;
                    return Ok(Reply::Value(json!(n)));
                }
                let mut cmd = doc! {"find": name, "filter": filter};
                if let Some(p) = arg(1).filter(|p| !p.is_null()) {
                    cmd.insert("projection", to_doc(Some(p))?);
                }
                let mut limit = (call.method == "findOne").then_some(1);
                for c in chain {
                    let a = c.args.first();
                    let number = || {
                        a.and_then(Value::as_i64)
                            .ok_or_else(|| DbError::new(format!("{}() expects a number.", c.method)))
                    };
                    match c.method.as_str() {
                        "sort" => _ = cmd.insert("sort", to_doc(a)?),
                        "projection" | "project" => _ = cmd.insert("projection", to_doc(a)?),
                        "skip" => _ = cmd.insert("skip", number()?),
                        "limit" => limit = Some(number()?.abs()),
                        "hint" => _ = cmd.insert("hint", to_bson(a.unwrap_or(&Value::Null))?),
                        "collation" => _ = cmd.insert("collation", to_doc(a)?),
                        "maxTimeMS" => _ = cmd.insert("maxTimeMS", number()?),
                        "batchSize" => _ = cmd.insert("batchSize", number()? as i32),
                        "allowDiskUse" => _ = cmd.insert("allowDiskUse", a.and_then(Value::as_bool).unwrap_or(true)),
                        "toArray" | "pretty" => {}
                        other => return Err(DbError::new(format!("Cursor method {other}() is not supported."))),
                    }
                }
                if let Some(l) = limit {
                    cmd.insert("limit", l);
                }
                let max = limit.map_or(max, |l| max.min(l as usize));
                self.cursor(db, cmd, max).await
            }
            "aggregate" => {
                let pipeline: Vec<Bson> = to_docs(arg(0))?.into_iter().map(Bson::Document).collect();
                let mut cmd = doc! {"aggregate": name, "pipeline": pipeline, "cursor": {}};
                cmd.extend(options(1)?);
                self.cursor(db, cmd, max).await
            }
            "countDocuments" | "count" => {
                Ok(Reply::Value(json!(coll.count_documents(to_doc(arg(0))?).await.map_err(err)?)))
            }
            "estimatedDocumentCount" => Ok(Reply::Value(json!(coll.estimated_document_count().await.map_err(err)?))),
            "distinct" => {
                let Some(Value::String(field)) = arg(0) else {
                    return Err(DbError::new("distinct() expects a field name."));
                };
                let values = coll.distinct(field, to_doc(arg(1))?).await.map_err(err)?;
                Ok(Reply::Value(Value::Array(values.into_iter().map(Bson::into_relaxed_extjson).collect())))
            }
            "insertOne" | "insert" if arg(0).is_some_and(Value::is_object) => {
                coll.insert_one(to_doc(arg(0))?).await.map_err(err)?;
                written(1)
            }
            "insertMany" | "insert" => {
                let docs = to_docs(arg(0))?;
                let r = coll.insert_many(docs).await.map_err(err)?;
                written(r.inserted_ids.len() as u64)
            }
            "updateOne" | "updateMany" => {
                let filter = to_doc(arg(0))?;
                let update: mongodb::options::UpdateModifications = match arg(1) {
                    Some(Value::Array(_)) => to_docs(arg(1))?.into(),
                    other => to_doc(other)?.into(),
                };
                let upsert = options(2)?.get_bool("upsert").unwrap_or(false);
                let r = if call.method == "updateOne" {
                    coll.update_one(filter, update).upsert(upsert).await
                } else {
                    coll.update_many(filter, update).upsert(upsert).await
                }
                .map_err(err)?;
                written(r.modified_count + u64::from(r.upserted_id.is_some()))
            }
            "replaceOne" => {
                let upsert = options(2)?.get_bool("upsert").unwrap_or(false);
                let r = coll.replace_one(to_doc(arg(0))?, to_doc(arg(1))?).upsert(upsert).await.map_err(err)?;
                written(r.modified_count + u64::from(r.upserted_id.is_some()))
            }
            "deleteOne" => written(coll.delete_one(to_doc(arg(0))?).await.map_err(err)?.deleted_count),
            "deleteMany" | "remove" => written(coll.delete_many(to_doc(arg(0))?).await.map_err(err)?.deleted_count),
            "createIndex" => {
                let keys = to_doc(arg(0))?;
                let mut index = doc! {"key": keys.clone(), "name": index_name(&keys)};
                index.extend(options(1)?);
                db.run_command(doc! {"createIndexes": name, "indexes": [index]}).await.map_err(err)?;
                written(0)
            }
            "dropIndex" => {
                let index = to_bson(arg(0).unwrap_or(&Value::Null))?;
                db.run_command(doc! {"dropIndexes": name, "index": index}).await.map_err(err)?;
                written(0)
            }
            "dropIndexes" => {
                db.run_command(doc! {"dropIndexes": name, "index": "*"}).await.map_err(err)?;
                written(0)
            }
            "getIndexes" => self.cursor(db, doc! {"listIndexes": name}, max).await,
            "drop" => {
                coll.drop().await.map_err(err)?;
                written(0)
            }
            "renameCollection" => {
                let Some(Value::String(to)) = arg(0) else {
                    return Err(DbError::new("renameCollection() expects the new name."));
                };
                let from = format!("{}.{name}", db.name());
                let to = format!("{}.{to}", db.name());
                self.client
                    .database("admin")
                    .run_command(doc! {"renameCollection": from, "to": to})
                    .await
                    .map_err(err)?;
                written(0)
            }
            "stats" => self.command(db, doc! {"collStats": name}).await,
            other => {
                Err(DbError::new(format!("{other}() is not supported here; db.runCommand({{…}}) runs any command.")))
            }
        }
    }
}

#[async_trait]
impl Connection for MongoConnection {
    fn kind(&self) -> DbKind {
        DbKind::MongoDb
    }

    async fn server_version(&self) -> DbResult<String> {
        let info = self.client.database("admin").run_command(doc! {"buildInfo": 1}).await.map_err(err)?;
        Ok(format!("MongoDB {}", info.get_str("version").unwrap_or_default()))
    }

    /// The server's databases, or just the connection's own when listing is not allowed.
    async fn databases(&self) -> DbResult<Vec<String>> {
        if self.cfg.only_default_database {
            return Ok(vec![self.default_db.clone()]);
        }
        let mut names = match self.client.list_database_names().await {
            Ok(n) => n,
            Err(_) => vec![self.default_db.clone()],
        };
        if !names.contains(&self.default_db) && !self.cfg.database.is_empty() {
            names.push(self.default_db.clone());
        }
        names.sort();
        Ok(names)
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        Ok(vec![database.to_string()])
    }

    async fn objects(&self, database: &str, _schema: &str) -> DbResult<Vec<DbObject>> {
        let specs: Vec<_> =
            self.db(Some(database)).list_collections().await.map_err(err)?.try_collect().await.map_err(err)?;
        // `system.*` collections are the server's own (`system.views` holds the views).
        let mut out: Vec<DbObject> = specs
            .into_iter()
            .filter(|s| !s.name.starts_with("system."))
            .map(|s| {
                let kind =
                    if s.collection_type == CollectionType::View { ObjectKind::View } else { ObjectKind::Collection };
                let detail = (s.collection_type == CollectionType::Timeseries).then(|| "time series".to_string());
                DbObject { kind, path: ObjectPath::new(database, database, s.name), detail, comment: None }
            })
            .collect();
        out.sort_by(|a, b| a.path.name.cmp(&b.path.name));
        Ok(out)
    }

    /// The fields found in a sample of documents, and the indexes.
    async fn table_details(&self, path: &ObjectPath) -> DbResult<TableDetails> {
        let db = self.db(Some(&path.database));
        let coll = db.collection::<Document>(&path.name);
        let docs: Vec<Document> =
            coll.find(doc! {}).limit(SAMPLE).await.map_err(err)?.try_collect().await.map_err(err)?;
        let docs: Vec<Value> = docs.into_iter().map(json_of).collect();
        let sample = m::documents_result(&docs, false);
        let columns = sample
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| ColumnInfo {
                name: c.name.clone(),
                data_type: c.type_name.clone(),
                nullable: sample.rows.iter().any(|r| r[i].is_null()),
                primary_key: c.name == "_id",
                ..Default::default()
            })
            .collect();
        let Reply::Documents { docs: indexes, .. } =
            self.cursor(&db, doc! {"listIndexes": &path.name}, usize::MAX).await.unwrap_or(Reply::Value(Value::Null))
        else {
            return Ok(TableDetails { columns, ..Default::default() });
        };
        let indexes = indexes
            .iter()
            .map(|ix| {
                let name = ix.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let columns =
                    ix.get("key").and_then(Value::as_object).map(|k| k.keys().cloned().collect()).unwrap_or_default();
                IndexInfo {
                    primary: name == "_id_",
                    unique: name == "_id_" || ix.get("unique").and_then(Value::as_bool).unwrap_or(false),
                    name,
                    columns,
                    ..Default::default()
                }
            })
            .collect();
        Ok(TableDetails { columns, indexes, ..Default::default() })
    }

    /// The shell statements that make the collection (or view) and its indexes.
    async fn ddl(&self, object: &DbObject) -> DbResult<String> {
        let db = self.db(Some(&object.path.database));
        let name = &object.path.name;
        let Reply::Documents { docs, .. } =
            self.cursor(&db, doc! {"listCollections": 1, "filter": {"name": name}}, 1).await?
        else {
            return Err(DbError::new("The collection was not found."));
        };
        let info = docs.into_iter().next().ok_or_else(|| DbError::new("The collection was not found."))?;
        let options = info.get("options").cloned().unwrap_or(json!({}));
        let quoted = Value::String(name.clone()).to_string();
        let mut out = if options.as_object().is_some_and(|o| o.is_empty()) {
            format!("db.createCollection({quoted});\n")
        } else {
            format!("db.createCollection({quoted}, {});\n", m::to_shell(&options))
        };
        if let Reply::Documents { docs: indexes, .. } = self.cursor(&db, doc! {"listIndexes": name}, usize::MAX).await?
        {
            for mut ix in indexes {
                let Some(o) = ix.as_object_mut() else { continue };
                if o.get("name").and_then(Value::as_str) == Some("_id_") {
                    continue;
                }
                let key = o.remove("key").unwrap_or(json!({}));
                o.remove("v");
                out.push_str(&format!(
                    "db.getCollection({quoted}).createIndex({}, {});\n",
                    m::to_shell(&key),
                    m::to_shell(&ix)
                ));
            }
        }
        Ok(out)
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let statement = m::parse(sql).map_err(DbError::new)?;
        let reply = self.mongo(database, &statement, max_rows).await?;
        Ok(m::outcome(reply))
    }

    /// Statements run one at a time and end at their own `maxTimeMS`.
    async fn cancel(&self) -> DbResult<()> {
        Ok(())
    }

    async fn mongo(&self, database: Option<&str>, statement: &Shell, max_docs: usize) -> DbResult<Reply> {
        let db = self.db(database);
        match statement {
            Shell::Show(what) => match what.as_str() {
                "dbs" | "databases" => {
                    let specs = self.client.list_databases().await.map_err(err)?;
                    let docs = specs
                        .into_iter()
                        .map(|s| json!({"name": s.name, "sizeOnDisk": s.size_on_disk, "empty": s.empty}))
                        .collect();
                    Ok(Reply::Documents { docs, more: false })
                }
                "collections" | "tables" => {
                    let mut names = db.list_collection_names().await.map_err(err)?;
                    names.sort();
                    Ok(Reply::Documents { docs: names.into_iter().map(|n| json!({"name": n})).collect(), more: false })
                }
                other => Err(DbError::new(format!("show {other} is not supported: try show dbs or show collections."))),
            },
            Shell::Db(call) => self.db_call(&db, call, max_docs).await,
            Shell::Collection { name, call, chain } => self.collection_call(&db, name, call, chain, max_docs).await,
        }
    }
}

/// A document's `_id` as a filter.
pub fn id_filter(doc: &Value) -> DbResult<Value> {
    doc.get("_id").map(|id| json!({"_id": id})).ok_or_else(|| DbError::new("The document has no _id."))
}

/// The documents of a collection, `skip`ped and limited, for the documents tab.
pub fn find_statement(
    collection: &str,
    filter: &Value,
    projection: &Value,
    sort: &Value,
    skip: u64,
    limit: u64,
) -> Shell {
    let mut chain = Vec::new();
    if sort.as_object().is_some_and(|s| !s.is_empty()) {
        chain.push(Call { method: "sort".into(), args: vec![sort.clone()] });
    }
    if skip > 0 {
        chain.push(Call { method: "skip".into(), args: vec![json!(skip)] });
    }
    chain.push(Call { method: "limit".into(), args: vec![json!(limit)] });
    let mut args = vec![filter.clone()];
    if projection.as_object().is_some_and(|p| !p.is_empty()) {
        args.push(projection.clone());
    }
    Shell::Collection { name: collection.into(), call: Call { method: "find".into(), args }, chain }
}
