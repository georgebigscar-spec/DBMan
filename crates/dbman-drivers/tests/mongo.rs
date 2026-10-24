//! MongoDB: shell statements through the console path, the explorer's
//! databases, collections and fields, writes, and a read-only connection
//! refusing them. Needs `DBMAN_TEST_MONGODB` (`mongodb://[user:pass@]host:port`);
//! skipped otherwise. Works in the database `dbman_test`, which it drops.

use dbman_core::mongo::{self, Reply};
use dbman_core::{Cell, ConnectionConfig, DbKind, DbObject, ObjectKind, ObjectPath, StatementOutcome};
use serde_json::json;

const DB: &str = "dbman_test";

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBMAN_TEST_MONGODB").ok().filter(|u| !u.is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, addr) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let addr = addr.trim_end_matches('/');
    let (host, port) = addr.split_once(':').unwrap_or((addr, "27017"));
    let mut cfg = ConnectionConfig::new(DbKind::MongoDb);
    cfg.host = host.into();
    cfg.port = port.parse().unwrap_or(27017);
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = DB.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    Some(cfg)
}

fn rows(o: StatementOutcome) -> (Vec<String>, Vec<Vec<Cell>>) {
    match o {
        StatementOutcome::Rows(r) => (r.columns.into_iter().map(|c| c.name).collect(), r.rows),
        other => panic!("{other:?}"),
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

#[tokio::test]
async fn mongodb() {
    let Some(cfg) = config() else { return };
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    assert!(c.server_version().await.unwrap().starts_with("MongoDB "));
    let run = |s: &'static str| {
        let c = c.clone();
        async move { c.execute(None, s, 100).await.unwrap_or_else(|e| panic!("{s}: {e}")) }
    };
    run("db.dropDatabase()").await;

    // Writes from the console.
    let n = run("db.people.insertMany([
           {name: 'Ann', age: 31, city: 'Berlin', tags: ['a', 'b'], joined: ISODate('2024-05-01')},
           {name: 'Bob', age: 25, city: 'Paris'},
           {name: 'Cid', age: 40, city: 'Berlin', address: {zip: '10115'}}
         ])")
    .await;
    assert!(matches!(n, StatementOutcome::Done { affected: Some(3) }), "{n:?}");
    run("db.people.createIndex({city: 1, age: -1}, {unique: false})").await;
    let n = run("db.people.updateOne({name: 'Bob'}, {$set: {age: 26}})").await;
    assert!(matches!(n, StatementOutcome::Done { affected: Some(1) }), "{n:?}");

    // Reads: a find with its cursor methods, counts, distinct and a pipeline.
    let (cols, r) =
        rows(run("db.people.find({age: {$gt: 25}}, {_id: 0, name: 1, age: 1}).sort({age: -1}).limit(2)").await);
    assert_eq!(cols, ["name", "age"]);
    assert_eq!(r, [[text("Cid"), text("40")], [text("Ann"), text("31")]]);
    let (_, r) = rows(run("db.people.countDocuments({city: 'Berlin'})").await);
    assert_eq!(r, [[text("2")]]);
    let (_, r) = rows(run("db.people.find({city: /^ber/i}).count()").await);
    assert_eq!(r, [[text("2")]]);
    let (_, r) = rows(run("db.people.distinct('city')").await);
    assert_eq!(r, [[text("Berlin")], [text("Paris")]]);
    let (cols, r) = rows(
        run("db.people.aggregate([{$group: {_id: '$city', n: {$sum: 1}, avg: {$avg: '$age'}}}, {$sort: {_id: 1}}])")
            .await,
    );
    assert_eq!(cols, ["_id", "n", "avg"]);
    assert_eq!(r[0][..2], [text("Berlin"), text("2")]);
    let (cols, r) = rows(run("db.people.find({name: 'Ann'})").await);
    let at = |name: &str| cols.iter().position(|c| c == name).unwrap();
    assert_eq!(r[0][at("joined")], text("2024-05-01T00:00:00Z"));
    assert_eq!(r[0][at("tags")], text("[\"a\",\"b\"]"));
    assert_eq!(cols[0], "_id");
    let (_, r) = rows(run("show collections").await);
    assert_eq!(r, [[text("people")]]);
    let (_, r) = rows(run("db.runCommand({ping: 1})").await);
    assert_eq!(r, [[text("1.0")]]);
    assert!(c.execute(None, "db.people.find({", 10).await.is_err());

    // The explorer: databases, collections and their fields.
    assert!(c.databases().await.unwrap().contains(&DB.to_string()));
    run("db.createView('berliners', 'people', [{$match: {city: 'Berlin'}}])").await;
    let objects = c.objects(DB, DB).await.unwrap();
    let kinds: Vec<(&str, ObjectKind)> = objects.iter().map(|o| (o.path.name.as_str(), o.kind)).collect();
    assert_eq!(kinds, [("berliners", ObjectKind::View), ("people", ObjectKind::Collection)]);
    let path = ObjectPath::new(DB, DB, "people");
    let details = c.table_details(&path).await.unwrap();
    let fields: Vec<(&str, &str)> = details.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(fields[..4], [("_id", "objectId"), ("name", "string"), ("age", "int"), ("city", "string")]);
    assert!(details.columns[0].primary_key && !details.columns[1].nullable && details.columns[4].nullable);
    let names: Vec<&str> = details.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["_id_", "city_1_age_-1"]);
    let object = DbObject { kind: ObjectKind::Collection, path: path.clone(), detail: None, comment: None };
    let ddl = c.ddl(&object).await.unwrap();
    assert!(ddl.contains("db.createCollection(\"people\")"), "{ddl}");
    assert!(ddl.contains("createIndex({\n  \"city\": 1,\n  \"age\": -1\n}"), "{ddl}");

    // The documents tab's own calls: a page of documents, then a document replaced by its _id.
    let page = dbman_drivers::mongo::find_statement("people", &json!({}), &json!({}), &json!({"age": 1}), 1, 1);
    let Reply::Documents { docs, more } = c.mongo(None, &page, 1).await.unwrap() else { panic!() };
    assert_eq!((docs.len(), more, docs[0]["name"].as_str()), (1, false, Some("Ann")));
    let mut ann = docs[0].clone();
    ann["age"] = json!(32);
    let filter = dbman_drivers::mongo::id_filter(&ann).unwrap();
    let replace = mongo::Shell::Collection {
        name: "people".into(),
        call: mongo::Call { method: "replaceOne".into(), args: vec![filter, ann] },
        chain: vec![],
    };
    assert_eq!(c.mongo(None, &replace, 0).await.unwrap(), Reply::Written { affected: 1 });
    let (_, r) = rows(run("db.people.findOne({name: 'Ann'}, {_id: 0, age: 1})").await);
    assert_eq!(r, [[text("32")]]);

    // Read-only connections read but do not write.
    let mut ro = cfg.clone();
    ro.read_only = true;
    let r = dbman_drivers::connect(&ro).await.unwrap();
    assert!(r.execute(None, "db.people.find()", 10).await.is_ok());
    assert!(r.execute(None, "db.people.deleteMany({})", 10).await.is_err());
    assert!(r.execute(None, "db.people.aggregate([{$out: 'copy'}])", 10).await.is_err());

    let n = run("db.people.deleteMany({city: 'Berlin'})").await;
    assert!(matches!(n, StatementOutcome::Done { affected: Some(2) }), "{n:?}");
    run("db.dropDatabase()").await;
}
