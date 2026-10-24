//! MongoDB aggregation pipelines built a stage at a time: the stages and
//! operators to pick from, a pipeline's stages as typed in the shell, the
//! pipeline up to a stage (for its preview), and the finished pipeline as code
//! in the languages the official drivers speak.

use serde_json::{Map, Value};

use crate::mongo;

/// A stage the builder offers: its name, what it does, and a body to start from.
pub struct StageInfo {
    pub name: &'static str,
    pub about: &'static str,
    pub template: &'static str,
}

const fn stage(name: &'static str, about: &'static str, template: &'static str) -> StageInfo {
    StageInfo { name, about, template }
}

/// The stages of `aggregate()`, the commonest first.
pub const STAGES: &[StageInfo] = &[
    stage("$match", "Keeps the documents that match a query", "{ field: \"value\" }"),
    stage("$project", "Keeps, drops, renames or computes fields", "{ _id: 0, field: 1 }"),
    stage("$group", "Groups documents by a key and accumulates each group", "{ _id: \"$field\", count: { $sum: 1 } }"),
    stage("$sort", "Orders the documents", "{ field: -1 }"),
    stage("$limit", "Passes the first n documents", "10"),
    stage("$skip", "Skips the first n documents", "10"),
    stage("$unwind", "One document per element of an array", "{ path: \"$items\", preserveNullAndEmptyArrays: false }"),
    stage(
        "$lookup",
        "Joins documents of another collection",
        "{ from: \"other\", localField: \"field\", foreignField: \"_id\", as: \"joined\" }",
    ),
    stage("$addFields", "Adds computed fields", "{ total: { $add: [\"$a\", \"$b\"] } }"),
    stage("$set", "Adds or replaces fields (alias of $addFields)", "{ field: \"value\" }"),
    stage("$unset", "Drops fields", "[\"field\"]"),
    stage("$count", "Counts the documents into one field", "\"count\""),
    stage("$sortByCount", "Groups by an expression and sorts by the count", "\"$field\""),
    stage("$replaceRoot", "Replaces each document with an embedded one", "{ newRoot: \"$embedded\" }"),
    stage("$replaceWith", "Replaces each document with an expression", "\"$embedded\""),
    stage(
        "$bucket",
        "Groups into ranges given by boundaries",
        "{ groupBy: \"$field\", boundaries: [0, 100, 200], default: \"other\", output: { count: { $sum: 1 } } }",
    ),
    stage("$bucketAuto", "Groups into a number of evenly filled ranges", "{ groupBy: \"$field\", buckets: 5 }"),
    stage("$facet", "Runs several pipelines on the same input", "{ byField: [{ $sortByCount: \"$field\" }] }"),
    stage("$sample", "Picks documents at random", "{ size: 5 }"),
    stage("$unionWith", "Adds the documents of another collection", "{ coll: \"other\", pipeline: [] }"),
    stage(
        "$graphLookup",
        "Follows references recursively",
        "{ from: \"other\", startWith: \"$parent\", connectFromField: \"parent\", connectToField: \"_id\", as: \"ancestors\" }",
    ),
    stage(
        "$setWindowFields",
        "Computes over a window of sorted documents",
        "{ partitionBy: \"$field\", sortBy: { date: 1 }, output: { running: { $sum: \"$amount\", window: { documents: [\"unbounded\", \"current\"] } } } }",
    ),
    stage(
        "$densify",
        "Fills gaps in a sequence of values",
        "{ field: \"date\", range: { step: 1, unit: \"day\", bounds: \"full\" } }",
    ),
    stage("$fill", "Fills null and missing values", "{ sortBy: { date: 1 }, output: { value: { method: \"locf\" } } }"),
    stage(
        "$geoNear",
        "Orders by distance from a point",
        "{ near: { type: \"Point\", coordinates: [0, 0] }, distanceField: \"distance\", spherical: true }",
    ),
    stage(
        "$redact",
        "Prunes documents by their own content",
        "{ $cond: { if: true, then: \"$$DESCEND\", else: \"$$PRUNE\" } }",
    ),
    stage("$collStats", "Statistics of the collection", "{ storageStats: {} }"),
    stage("$indexStats", "How much each index is used", "{}"),
    stage("$planCacheStats", "The plan cache of the collection", "{}"),
    stage("$out", "Writes the result to a collection", "\"output\""),
    stage(
        "$merge",
        "Merges the result into a collection",
        "{ into: \"output\", whenMatched: \"merge\", whenNotMatched: \"insert\" }",
    ),
    stage(
        "$search",
        "Atlas Search full-text query",
        "{ index: \"default\", text: { query: \"word\", path: \"field\" } }",
    ),
    stage(
        "$searchMeta",
        "Atlas Search metadata",
        "{ index: \"default\", facet: { operator: { exists: { path: \"field\" } } } }",
    ),
    stage(
        "$vectorSearch",
        "Atlas Vector Search",
        "{ index: \"vector_index\", path: \"embedding\", queryVector: [0.1, 0.2], numCandidates: 100, limit: 10 }",
    ),
];

/// The stage the builder knows by `name`.
pub fn stage_info(name: &str) -> Option<&'static StageInfo> {
    STAGES.iter().find(|s| s.name == name)
}

/// An operator to insert into a stage's body.
pub struct Operator {
    pub name: &'static str,
    pub group: &'static str,
    /// What is inserted; `$x` here means fields of the document.
    pub snippet: &'static str,
}

const fn op(group: &'static str, name: &'static str, snippet: &'static str) -> Operator {
    Operator { name, group, snippet }
}

/// The operators usable inside the stages: query, expression, accumulator
/// and window operators, with a snippet for each.
pub const OPERATORS: &[Operator] = &[
    // Query.
    op("Query", "$eq", "{ $eq: \"value\" }"),
    op("Query", "$ne", "{ $ne: \"value\" }"),
    op("Query", "$gt", "{ $gt: 0 }"),
    op("Query", "$gte", "{ $gte: 0 }"),
    op("Query", "$lt", "{ $lt: 0 }"),
    op("Query", "$lte", "{ $lte: 0 }"),
    op("Query", "$in", "{ $in: [\"a\", \"b\"] }"),
    op("Query", "$nin", "{ $nin: [\"a\", \"b\"] }"),
    op("Query", "$and", "$and: [{}, {}]"),
    op("Query", "$or", "$or: [{}, {}]"),
    op("Query", "$nor", "$nor: [{}, {}]"),
    op("Query", "$not", "{ $not: { $gt: 0 } }"),
    op("Query", "$exists", "{ $exists: true }"),
    op("Query", "$type", "{ $type: \"string\" }"),
    op("Query", "$expr", "$expr: { $gt: [\"$a\", \"$b\"] }"),
    op("Query", "$jsonSchema", "$jsonSchema: { required: [\"field\"] }"),
    op("Query", "$mod", "{ $mod: [2, 0] }"),
    op("Query", "$regex", "{ $regex: \"^abc\", $options: \"i\" }"),
    op("Query", "$text", "$text: { $search: \"word\" }"),
    op("Query", "$where", "$where: \"this.a > this.b\""),
    op("Query", "$all", "{ $all: [\"a\", \"b\"] }"),
    op("Query", "$elemMatch", "{ $elemMatch: { field: \"value\" } }"),
    op("Query", "$size", "{ $size: 2 }"),
    op("Query", "$geoWithin", "{ $geoWithin: { $centerSphere: [[0, 0], 0.01] } }"),
    op("Query", "$geoIntersects", "{ $geoIntersects: { $geometry: { type: \"Point\", coordinates: [0, 0] } } }"),
    op("Query", "$near", "{ $near: { $geometry: { type: \"Point\", coordinates: [0, 0] }, $maxDistance: 1000 } }"),
    op("Query", "$nearSphere", "{ $nearSphere: { $geometry: { type: \"Point\", coordinates: [0, 0] } } }"),
    op("Query", "$bitsAllSet", "{ $bitsAllSet: [1, 5] }"),
    op("Query", "$bitsAllClear", "{ $bitsAllClear: [1, 5] }"),
    op("Query", "$bitsAnySet", "{ $bitsAnySet: [1, 5] }"),
    op("Query", "$bitsAnyClear", "{ $bitsAnyClear: [1, 5] }"),
    // Arithmetic.
    op("Arithmetic", "$abs", "{ $abs: \"$x\" }"),
    op("Arithmetic", "$add", "{ $add: [\"$a\", \"$b\"] }"),
    op("Arithmetic", "$ceil", "{ $ceil: \"$x\" }"),
    op("Arithmetic", "$divide", "{ $divide: [\"$a\", \"$b\"] }"),
    op("Arithmetic", "$exp", "{ $exp: \"$x\" }"),
    op("Arithmetic", "$floor", "{ $floor: \"$x\" }"),
    op("Arithmetic", "$ln", "{ $ln: \"$x\" }"),
    op("Arithmetic", "$log", "{ $log: [\"$x\", 2] }"),
    op("Arithmetic", "$log10", "{ $log10: \"$x\" }"),
    op("Arithmetic", "$mod", "{ $mod: [\"$a\", \"$b\"] }"),
    op("Arithmetic", "$multiply", "{ $multiply: [\"$a\", \"$b\"] }"),
    op("Arithmetic", "$pow", "{ $pow: [\"$x\", 2] }"),
    op("Arithmetic", "$round", "{ $round: [\"$x\", 2] }"),
    op("Arithmetic", "$sqrt", "{ $sqrt: \"$x\" }"),
    op("Arithmetic", "$subtract", "{ $subtract: [\"$a\", \"$b\"] }"),
    op("Arithmetic", "$trunc", "{ $trunc: [\"$x\", 0] }"),
    // Array.
    op("Array", "$arrayElemAt", "{ $arrayElemAt: [\"$items\", 0] }"),
    op("Array", "$arrayToObject", "{ $arrayToObject: \"$pairs\" }"),
    op("Array", "$concatArrays", "{ $concatArrays: [\"$a\", \"$b\"] }"),
    op("Array", "$filter", "{ $filter: { input: \"$items\", as: \"item\", cond: { $gt: [\"$$item.qty\", 0] } } }"),
    op("Array", "$first", "{ $first: \"$items\" }"),
    op("Array", "$firstN", "{ $firstN: { input: \"$items\", n: 3 } }"),
    op("Array", "$in", "{ $in: [\"value\", \"$items\"] }"),
    op("Array", "$indexOfArray", "{ $indexOfArray: [\"$items\", \"value\"] }"),
    op("Array", "$isArray", "{ $isArray: \"$items\" }"),
    op("Array", "$last", "{ $last: \"$items\" }"),
    op("Array", "$lastN", "{ $lastN: { input: \"$items\", n: 3 } }"),
    op("Array", "$map", "{ $map: { input: \"$items\", as: \"item\", in: \"$$item.name\" } }"),
    op("Array", "$maxN", "{ $maxN: { input: \"$items\", n: 3 } }"),
    op("Array", "$minN", "{ $minN: { input: \"$items\", n: 3 } }"),
    op("Array", "$objectToArray", "{ $objectToArray: \"$doc\" }"),
    op("Array", "$range", "{ $range: [0, 10, 1] }"),
    op(
        "Array",
        "$reduce",
        "{ $reduce: { input: \"$items\", initialValue: 0, in: { $add: [\"$$value\", \"$$this\"] } } }",
    ),
    op("Array", "$reverseArray", "{ $reverseArray: \"$items\" }"),
    op("Array", "$size", "{ $size: \"$items\" }"),
    op("Array", "$slice", "{ $slice: [\"$items\", 3] }"),
    op("Array", "$sortArray", "{ $sortArray: { input: \"$items\", sortBy: { qty: -1 } } }"),
    op("Array", "$zip", "{ $zip: { inputs: [\"$a\", \"$b\"] } }"),
    // Boolean.
    op("Boolean", "$and", "{ $and: [true, true] }"),
    op("Boolean", "$not", "{ $not: [\"$flag\"] }"),
    op("Boolean", "$or", "{ $or: [true, false] }"),
    // Comparison.
    op("Comparison", "$cmp", "{ $cmp: [\"$a\", \"$b\"] }"),
    op("Comparison", "$eq", "{ $eq: [\"$a\", \"$b\"] }"),
    op("Comparison", "$gt", "{ $gt: [\"$a\", \"$b\"] }"),
    op("Comparison", "$gte", "{ $gte: [\"$a\", \"$b\"] }"),
    op("Comparison", "$lt", "{ $lt: [\"$a\", \"$b\"] }"),
    op("Comparison", "$lte", "{ $lte: [\"$a\", \"$b\"] }"),
    op("Comparison", "$ne", "{ $ne: [\"$a\", \"$b\"] }"),
    // Conditional.
    op("Conditional", "$cond", "{ $cond: { if: { $gt: [\"$x\", 0] }, then: \"yes\", else: \"no\" } }"),
    op("Conditional", "$ifNull", "{ $ifNull: [\"$x\", \"default\"] }"),
    op(
        "Conditional",
        "$switch",
        "{ $switch: { branches: [{ case: { $gt: [\"$x\", 0] }, then: \"positive\" }], default: \"other\" } }",
    ),
    // Custom.
    op(
        "Custom",
        "$accumulator",
        "{ $accumulator: { init: \"function() { return 0 }\", accumulate: \"function(s, x) { return s + x }\", accumulateArgs: [\"$x\"], merge: \"function(a, b) { return a + b }\", lang: \"js\" } }",
    ),
    op("Custom", "$function", "{ $function: { body: \"function(x) { return x }\", args: [\"$x\"], lang: \"js\" } }"),
    // Data size.
    op("Data size", "$binarySize", "{ $binarySize: \"$data\" }"),
    op("Data size", "$bsonSize", "{ $bsonSize: \"$$ROOT\" }"),
    // Date.
    op("Date", "$dateAdd", "{ $dateAdd: { startDate: \"$date\", unit: \"day\", amount: 1 } }"),
    op("Date", "$dateDiff", "{ $dateDiff: { startDate: \"$start\", endDate: \"$end\", unit: \"day\" } }"),
    op("Date", "$dateFromParts", "{ $dateFromParts: { year: 2024, month: 1, day: 1 } }"),
    op("Date", "$dateFromString", "{ $dateFromString: { dateString: \"$text\" } }"),
    op("Date", "$dateSubtract", "{ $dateSubtract: { startDate: \"$date\", unit: \"day\", amount: 1 } }"),
    op("Date", "$dateToParts", "{ $dateToParts: { date: \"$date\" } }"),
    op("Date", "$dateToString", "{ $dateToString: { format: \"%Y-%m-%d\", date: \"$date\" } }"),
    op("Date", "$dateTrunc", "{ $dateTrunc: { date: \"$date\", unit: \"month\" } }"),
    op("Date", "$dayOfMonth", "{ $dayOfMonth: \"$date\" }"),
    op("Date", "$dayOfWeek", "{ $dayOfWeek: \"$date\" }"),
    op("Date", "$dayOfYear", "{ $dayOfYear: \"$date\" }"),
    op("Date", "$hour", "{ $hour: \"$date\" }"),
    op("Date", "$isoDayOfWeek", "{ $isoDayOfWeek: \"$date\" }"),
    op("Date", "$isoWeek", "{ $isoWeek: \"$date\" }"),
    op("Date", "$isoWeekYear", "{ $isoWeekYear: \"$date\" }"),
    op("Date", "$millisecond", "{ $millisecond: \"$date\" }"),
    op("Date", "$minute", "{ $minute: \"$date\" }"),
    op("Date", "$month", "{ $month: \"$date\" }"),
    op("Date", "$second", "{ $second: \"$date\" }"),
    op("Date", "$toDate", "{ $toDate: \"$x\" }"),
    op("Date", "$week", "{ $week: \"$date\" }"),
    op("Date", "$year", "{ $year: \"$date\" }"),
    // Literal and miscellaneous.
    op("Miscellaneous", "$literal", "{ $literal: \"$notAField\" }"),
    op("Miscellaneous", "$getField", "{ $getField: { field: \"name\", input: \"$doc\" } }"),
    op("Miscellaneous", "$rand", "{ $rand: {} }"),
    op("Miscellaneous", "$sampleRate", "{ $sampleRate: 0.5 }"),
    op("Miscellaneous", "$toHashedIndexKey", "{ $toHashedIndexKey: \"$x\" }"),
    op(
        "Miscellaneous",
        "$let",
        "{ $let: { vars: { total: { $add: [\"$a\", \"$b\"] } }, in: { $multiply: [\"$$total\", 2] } } }",
    ),
    op("Miscellaneous", "$meta", "{ $meta: \"textScore\" }"),
    // Object.
    op("Object", "$mergeObjects", "{ $mergeObjects: [\"$a\", \"$b\"] }"),
    op("Object", "$setField", "{ $setField: { field: \"name\", input: \"$$ROOT\", value: \"value\" } }"),
    op("Object", "$unsetField", "{ $unsetField: { field: \"name\", input: \"$$ROOT\" } }"),
    // Set.
    op("Set", "$allElementsTrue", "{ $allElementsTrue: [\"$flags\"] }"),
    op("Set", "$anyElementTrue", "{ $anyElementTrue: [\"$flags\"] }"),
    op("Set", "$setDifference", "{ $setDifference: [\"$a\", \"$b\"] }"),
    op("Set", "$setEquals", "{ $setEquals: [\"$a\", \"$b\"] }"),
    op("Set", "$setIntersection", "{ $setIntersection: [\"$a\", \"$b\"] }"),
    op("Set", "$setIsSubset", "{ $setIsSubset: [\"$a\", \"$b\"] }"),
    op("Set", "$setUnion", "{ $setUnion: [\"$a\", \"$b\"] }"),
    // String.
    op("String", "$concat", "{ $concat: [\"$first\", \" \", \"$last\"] }"),
    op("String", "$indexOfBytes", "{ $indexOfBytes: [\"$text\", \"abc\"] }"),
    op("String", "$indexOfCP", "{ $indexOfCP: [\"$text\", \"abc\"] }"),
    op("String", "$ltrim", "{ $ltrim: { input: \"$text\" } }"),
    op("String", "$regexFind", "{ $regexFind: { input: \"$text\", regex: \"^abc\" } }"),
    op("String", "$regexFindAll", "{ $regexFindAll: { input: \"$text\", regex: \"\\\\d+\" } }"),
    op("String", "$regexMatch", "{ $regexMatch: { input: \"$text\", regex: \"^abc\", options: \"i\" } }"),
    op("String", "$replaceOne", "{ $replaceOne: { input: \"$text\", find: \"a\", replacement: \"b\" } }"),
    op("String", "$replaceAll", "{ $replaceAll: { input: \"$text\", find: \"a\", replacement: \"b\" } }"),
    op("String", "$rtrim", "{ $rtrim: { input: \"$text\" } }"),
    op("String", "$split", "{ $split: [\"$text\", \",\"] }"),
    op("String", "$strLenBytes", "{ $strLenBytes: \"$text\" }"),
    op("String", "$strLenCP", "{ $strLenCP: \"$text\" }"),
    op("String", "$strcasecmp", "{ $strcasecmp: [\"$a\", \"$b\"] }"),
    op("String", "$substr", "{ $substr: [\"$text\", 0, 3] }"),
    op("String", "$substrBytes", "{ $substrBytes: [\"$text\", 0, 3] }"),
    op("String", "$substrCP", "{ $substrCP: [\"$text\", 0, 3] }"),
    op("String", "$toLower", "{ $toLower: \"$text\" }"),
    op("String", "$toString", "{ $toString: \"$x\" }"),
    op("String", "$toUpper", "{ $toUpper: \"$text\" }"),
    op("String", "$trim", "{ $trim: { input: \"$text\" } }"),
    // Timestamp.
    op("Timestamp", "$tsIncrement", "{ $tsIncrement: \"$ts\" }"),
    op("Timestamp", "$tsSecond", "{ $tsSecond: \"$ts\" }"),
    // Trigonometry.
    op("Trigonometry", "$sin", "{ $sin: \"$x\" }"),
    op("Trigonometry", "$cos", "{ $cos: \"$x\" }"),
    op("Trigonometry", "$tan", "{ $tan: \"$x\" }"),
    op("Trigonometry", "$asin", "{ $asin: \"$x\" }"),
    op("Trigonometry", "$acos", "{ $acos: \"$x\" }"),
    op("Trigonometry", "$atan", "{ $atan: \"$x\" }"),
    op("Trigonometry", "$atan2", "{ $atan2: [\"$y\", \"$x\"] }"),
    op("Trigonometry", "$asinh", "{ $asinh: \"$x\" }"),
    op("Trigonometry", "$acosh", "{ $acosh: \"$x\" }"),
    op("Trigonometry", "$atanh", "{ $atanh: \"$x\" }"),
    op("Trigonometry", "$sinh", "{ $sinh: \"$x\" }"),
    op("Trigonometry", "$cosh", "{ $cosh: \"$x\" }"),
    op("Trigonometry", "$tanh", "{ $tanh: \"$x\" }"),
    op("Trigonometry", "$degreesToRadians", "{ $degreesToRadians: \"$x\" }"),
    op("Trigonometry", "$radiansToDegrees", "{ $radiansToDegrees: \"$x\" }"),
    // Type.
    op("Type", "$convert", "{ $convert: { input: \"$x\", to: \"int\", onError: null } }"),
    op("Type", "$isNumber", "{ $isNumber: \"$x\" }"),
    op("Type", "$toBool", "{ $toBool: \"$x\" }"),
    op("Type", "$toDecimal", "{ $toDecimal: \"$x\" }"),
    op("Type", "$toDouble", "{ $toDouble: \"$x\" }"),
    op("Type", "$toInt", "{ $toInt: \"$x\" }"),
    op("Type", "$toLong", "{ $toLong: \"$x\" }"),
    op("Type", "$toObjectId", "{ $toObjectId: \"$x\" }"),
    op("Type", "$toUUID", "{ $toUUID: \"$x\" }"),
    op("Type", "$type", "{ $type: \"$x\" }"),
    // Accumulators ($group, $setWindowFields).
    op("Accumulator", "$addToSet", "{ $addToSet: \"$x\" }"),
    op("Accumulator", "$avg", "{ $avg: \"$x\" }"),
    op("Accumulator", "$bottom", "{ $bottom: { sortBy: { x: 1 }, output: \"$x\" } }"),
    op("Accumulator", "$bottomN", "{ $bottomN: { n: 3, sortBy: { x: 1 }, output: \"$x\" } }"),
    op("Accumulator", "$count", "{ $count: {} }"),
    op("Accumulator", "$first", "{ $first: \"$x\" }"),
    op("Accumulator", "$firstN", "{ $firstN: { input: \"$x\", n: 3 } }"),
    op("Accumulator", "$last", "{ $last: \"$x\" }"),
    op("Accumulator", "$lastN", "{ $lastN: { input: \"$x\", n: 3 } }"),
    op("Accumulator", "$max", "{ $max: \"$x\" }"),
    op("Accumulator", "$maxN", "{ $maxN: { input: \"$x\", n: 3 } }"),
    op("Accumulator", "$median", "{ $median: { input: \"$x\", method: \"approximate\" } }"),
    op("Accumulator", "$mergeObjects", "{ $mergeObjects: \"$doc\" }"),
    op("Accumulator", "$min", "{ $min: \"$x\" }"),
    op("Accumulator", "$minN", "{ $minN: { input: \"$x\", n: 3 } }"),
    op("Accumulator", "$percentile", "{ $percentile: { input: \"$x\", p: [0.5, 0.95], method: \"approximate\" } }"),
    op("Accumulator", "$push", "{ $push: \"$x\" }"),
    op("Accumulator", "$stdDevPop", "{ $stdDevPop: \"$x\" }"),
    op("Accumulator", "$stdDevSamp", "{ $stdDevSamp: \"$x\" }"),
    op("Accumulator", "$sum", "{ $sum: \"$x\" }"),
    op("Accumulator", "$top", "{ $top: { sortBy: { x: -1 }, output: \"$x\" } }"),
    op("Accumulator", "$topN", "{ $topN: { n: 3, sortBy: { x: -1 }, output: \"$x\" } }"),
    // Window ($setWindowFields).
    op("Window", "$covariancePop", "{ $covariancePop: [\"$x\", \"$y\"] }"),
    op("Window", "$covarianceSamp", "{ $covarianceSamp: [\"$x\", \"$y\"] }"),
    op("Window", "$denseRank", "{ $denseRank: {} }"),
    op(
        "Window",
        "$derivative",
        "{ $derivative: { input: \"$x\", unit: \"hour\" }, window: { range: [-1, 0], unit: \"hour\" } }",
    ),
    op("Window", "$documentNumber", "{ $documentNumber: {} }"),
    op("Window", "$expMovingAvg", "{ $expMovingAvg: { input: \"$x\", N: 3 } }"),
    op(
        "Window",
        "$integral",
        "{ $integral: { input: \"$x\", unit: \"hour\" }, window: { range: [\"unbounded\", \"current\"], unit: \"hour\" } }",
    ),
    op("Window", "$linearFill", "{ $linearFill: \"$x\" }"),
    op("Window", "$locf", "{ $locf: \"$x\" }"),
    op("Window", "$rank", "{ $rank: {} }"),
    op("Window", "$shift", "{ $shift: { output: \"$x\", by: 1, default: null } }"),
    // Variables.
    op("Variable", "$$ROOT", "\"$$ROOT\""),
    op("Variable", "$$CURRENT", "\"$$CURRENT\""),
    op("Variable", "$$NOW", "\"$$NOW\""),
    op("Variable", "$$CLUSTER_TIME", "\"$$CLUSTER_TIME\""),
    op("Variable", "$$REMOVE", "\"$$REMOVE\""),
    op("Variable", "$$DESCEND", "\"$$DESCEND\""),
    op("Variable", "$$PRUNE", "\"$$PRUNE\""),
    op("Variable", "$$KEEP", "\"$$KEEP\""),
    op("Variable", "$$SEARCH_META", "\"$$SEARCH_META\""),
    op("Variable", "$$USER_ROLES", "\"$$USER_ROLES\""),
];

/// The operators whose name contains `query` (any case), each name once per group.
pub fn find_operators(query: &str) -> Vec<&'static Operator> {
    let q = query.trim().trim_start_matches('$').to_lowercase();
    OPERATORS
        .iter()
        .filter(|o| q.is_empty() || o.name.to_lowercase().contains(&q) || o.group.to_lowercase() == q)
        .collect()
}

// ---------------------------------------------------------------------------
// Pipelines

/// One stage as typed: its operator and its body in the shell's syntax.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub name: String,
    pub body: String,
    pub enabled: bool,
}

impl Stage {
    /// A stage of `name` with the builder's starting body.
    pub fn new(name: &str) -> Self {
        let body = stage_info(name).map_or("{}", |s| s.template).to_string();
        Self { name: name.into(), body, enabled: true }
    }

    /// The stage as a document, `{ $match: {...} }`.
    pub fn to_value(&self) -> Result<Value, String> {
        if !self.name.starts_with('$') || self.name.len() < 2 {
            return Err(format!("A stage is named with a $, such as $match, not {:?}.", self.name));
        }
        let body = mongo::parse_value(&self.body)?;
        let mut m = Map::new();
        m.insert(self.name.clone(), body);
        Ok(Value::Object(m))
    }

    /// Whether the stage writes to a collection, so it never runs in a preview.
    pub fn writes(&self) -> bool {
        self.name == "$out" || self.name == "$merge"
    }
}

/// The enabled stages from the first up to and including `upto`, as an array;
/// the error names the stage (counted from 0) that does not parse.
pub fn pipeline(stages: &[Stage], upto: usize) -> Result<Value, (usize, String)> {
    let mut out = Vec::new();
    for (i, s) in stages.iter().enumerate().take(upto + 1) {
        if s.enabled {
            out.push(s.to_value().map_err(|e| (i, e))?);
        }
    }
    Ok(Value::Array(out))
}

/// The pipeline for the preview of stage `upto`: the stages up to it, then a
/// `$limit` so that only a sample comes back. `None` when a stage up to it writes.
pub fn preview(stages: &[Stage], upto: usize, sample: u64) -> Result<Option<Value>, (usize, String)> {
    if stages.iter().take(upto + 1).any(|s| s.enabled && s.writes()) {
        return Ok(None);
    }
    let Value::Array(mut p) = pipeline(stages, upto)? else { unreachable!() };
    p.push(serde_json::json!({ "$limit": sample }));
    Ok(Some(Value::Array(p)))
}

/// The stages of a pipeline typed as a shell array, `[{ $match: … }, …]`.
pub fn stages_from_text(text: &str) -> Result<Vec<Stage>, String> {
    let Value::Array(items) = mongo::parse_value(text)? else {
        return Err("A pipeline is an array of stages: [{ $match: … }, …].".into());
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Object(o) if o.len() == 1 => {
                let (name, body) = o.into_iter().next().unwrap();
                Ok(Stage { name, body: body_text(&body), enabled: true })
            }
            other => Err(format!("A stage has exactly one field, such as {{ $match: … }}, not {other}.")),
        })
        .collect()
}

/// A stage's body as the editor shows it: short ones on one line.
pub fn body_text(v: &Value) -> String {
    compact(v, 60)
}

/// A value as the shell writes it, with every document and array that fits
/// in `width` characters on one line.
pub fn compact(v: &Value, width: usize) -> String {
    let mut out = String::new();
    write_compact(v, width, 0, &mut out);
    out
}

fn is_document(v: &Value) -> bool {
    matches!(v, Value::Object(o) if !o.is_empty() && mongo::special_text(o).is_none())
}

fn flat(v: &Value) -> String {
    match v {
        Value::Object(o) if is_document(v) => {
            let items: Vec<String> =
                o.iter().map(|(k, x)| format!("{}: {}", Value::String(k.clone()), flat(x))).collect();
            format!("{{ {} }}", items.join(", "))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(flat).collect::<Vec<_>>().join(", ")),
        other => mongo::to_shell(other),
    }
}

fn write_compact(v: &Value, width: usize, indent: usize, out: &mut String) {
    let one = flat(v);
    if one.len() + indent * 2 <= width {
        out.push_str(&one);
        return;
    }
    let pad = |n: usize| "  ".repeat(n);
    match v {
        Value::Object(o) if is_document(v) => {
            out.push_str("{\n");
            for (i, (k, x)) in o.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                out.push_str(&Value::String(k.clone()).to_string());
                out.push_str(": ");
                write_compact(x, width, indent + 1, out);
                out.push_str(if i + 1 < o.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
        Value::Array(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, x) in a.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write_compact(x, width, indent + 1, out);
                out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        _ => out.push_str(&one),
    }
}

// ---------------------------------------------------------------------------
// Code

/// The languages a pipeline is exported to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Shell,
    Python,
    Node,
    Java,
    CSharp,
    Go,
    Rust,
    Php,
    Ruby,
}

impl Lang {
    pub const ALL: [Lang; 9] =
        [Lang::Shell, Lang::Python, Lang::Node, Lang::Java, Lang::CSharp, Lang::Go, Lang::Rust, Lang::Php, Lang::Ruby];

    pub fn label(self) -> &'static str {
        match self {
            Lang::Shell => "mongosh",
            Lang::Python => "Python",
            Lang::Node => "Node.js",
            Lang::Java => "Java",
            Lang::CSharp => "C#",
            Lang::Go => "Go",
            Lang::Rust => "Rust",
            Lang::Php => "PHP",
            Lang::Ruby => "Ruby",
        }
    }

    /// The file extension for saving the code.
    pub fn extension(self) -> &'static str {
        match self {
            Lang::Shell | Lang::Node => "js",
            Lang::Python => "py",
            Lang::Java => "java",
            Lang::CSharp => "cs",
            Lang::Go => "go",
            Lang::Rust => "rs",
            Lang::Php => "php",
            Lang::Ruby => "rb",
        }
    }
}

/// What the code uses, for its imports.
#[derive(Default)]
struct Uses {
    object_id: bool,
    date: bool,
    long: bool,
    decimal: bool,
    regex: bool,
}

/// A date as (year, month, day, hour, minute, second, millisecond).
fn date_parts(iso: &str) -> Option<[i64; 7]> {
    let n = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let ms = iso.get(19..).and_then(|s| s.strip_prefix('.')).map_or(Some(0), |s| {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        format!("{digits:0<3}")[..3].parse().ok()
    })?;
    Some([n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?, ms])
}

struct Writer {
    lang: Lang,
    uses: Uses,
}

impl Writer {
    /// One level of indentation, as the language's own style has it.
    fn pad(&self, levels: usize) -> String {
        let unit = match self.lang {
            Lang::Python | Lang::Java | Lang::CSharp | Lang::Rust | Lang::Php => 4,
            _ => 2,
        };
        " ".repeat(unit * levels)
    }

    fn string(&self, s: &str) -> String {
        match self.lang {
            Lang::Php | Lang::Ruby => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Lang::Rust => {
                let json = Value::String(s.to_string()).to_string();
                // JSON's `\u001f` is `\u{1f}` in Rust.
                let mut out = String::new();
                let mut rest = json.as_str();
                while let Some(i) = rest.find("\\u") {
                    out.push_str(&rest[..i]);
                    out.push_str(&format!("\\u{{{}}}", &rest[i + 2..i + 6]));
                    rest = &rest[i + 6..];
                }
                out.push_str(rest);
                out
            }
            _ => Value::String(s.to_string()).to_string(),
        }
    }

    fn null(&self) -> &'static str {
        match self.lang {
            Lang::Python => "None",
            Lang::CSharp => "BsonNull.Value",
            Lang::Go => "nil",
            Lang::Ruby => "nil",
            _ => "null",
        }
    }

    fn boolean(&self, b: bool) -> &'static str {
        match (self.lang, b) {
            (Lang::Python, true) => "True",
            (Lang::Python, false) => "False",
            (_, true) => "true",
            (_, false) => "false",
        }
    }

    /// A wrapped Extended JSON scalar in the language's own constructor.
    fn special(&mut self, o: &Map<String, Value>) -> Option<String> {
        if o.len() != 1 {
            return None;
        }
        let (k, v) = o.iter().next()?;
        let l = self.lang;
        Some(match (k.as_str(), v) {
            ("$oid", Value::String(id)) => {
                self.uses.object_id = true;
                let q = self.string(id);
                match l {
                    Lang::Shell => format!("ObjectId({q})"),
                    Lang::Python => format!("ObjectId({q})"),
                    Lang::Node | Lang::Java | Lang::CSharp => format!("new ObjectId({q})"),
                    Lang::Go => format!("oid({q})"),
                    Lang::Rust => format!("(ObjectId::parse_str({q}).unwrap())"),
                    Lang::Php => format!("new MongoDB\\BSON\\ObjectId({q})"),
                    Lang::Ruby => format!("BSON::ObjectId({q})"),
                }
            }
            ("$date", _) => {
                self.uses.date = true;
                let iso = mongo::special_text(o)?;
                let q = self.string(&iso);
                let [y, mo, d, h, mi, s, ms] = date_parts(&iso)?;
                match l {
                    Lang::Shell => format!("ISODate({q})"),
                    Lang::Python if ms == 0 => format!("datetime({y}, {mo}, {d}, {h}, {mi}, {s}, tzinfo=timezone.utc)"),
                    Lang::Python => {
                        format!("datetime({y}, {mo}, {d}, {h}, {mi}, {s}, {}, tzinfo=timezone.utc)", ms * 1000)
                    }
                    Lang::Node => format!("new Date({q})"),
                    Lang::Java => format!("Date.from(Instant.parse({q}))"),
                    Lang::CSharp => format!("new DateTime({y}, {mo}, {d}, {h}, {mi}, {s}, {ms}, DateTimeKind.Utc)"),
                    Lang::Go => format!("time.Date({y}, {mo}, {d}, {h}, {mi}, {s}, {}, time.UTC)", ms * 1_000_000),
                    Lang::Rust => format!("(DateTime::parse_rfc3339_str({q}).unwrap())"),
                    Lang::Php => format!("new MongoDB\\BSON\\UTCDateTime(new DateTimeImmutable({q}))"),
                    Lang::Ruby if ms == 0 => format!("Time.utc({y}, {mo}, {d}, {h}, {mi}, {s})"),
                    Lang::Ruby => format!("Time.utc({y}, {mo}, {d}, {h}, {mi}, {s}, {})", ms * 1000),
                }
            }
            ("$numberLong", Value::String(n)) => {
                self.uses.long = true;
                match l {
                    Lang::Shell => format!("NumberLong({})", self.string(n)),
                    Lang::Python => format!("Int64({n})"),
                    Lang::Node => format!("Long.fromString({})", self.string(n)),
                    Lang::Java | Lang::CSharp => format!("{n}L"),
                    Lang::Go => format!("int64({n})"),
                    Lang::Rust => format!("{n}_i64"),
                    Lang::Php | Lang::Ruby => n.clone(),
                }
            }
            ("$numberInt" | "$numberDouble", Value::String(n)) => n.clone(),
            ("$numberDecimal", Value::String(n)) => {
                self.uses.decimal = true;
                let q = self.string(n);
                match l {
                    Lang::Shell => format!("NumberDecimal({q})"),
                    Lang::Python => format!("Decimal128({q})"),
                    Lang::Node => format!("Decimal128.fromString({q})"),
                    Lang::Java => format!("Decimal128.parse({q})"),
                    Lang::CSharp => format!("Decimal128.Parse({q})"),
                    Lang::Go => format!("dec({q})"),
                    Lang::Rust => format!("({q}.parse::<Decimal128>().unwrap())"),
                    Lang::Php => format!("new MongoDB\\BSON\\Decimal128({q})"),
                    Lang::Ruby => format!("BSON::Decimal128.new({q})"),
                }
            }
            ("$regularExpression", Value::Object(r)) => {
                self.uses.regex = true;
                let p = self.string(r.get("pattern")?.as_str()?);
                let f = self.string(r.get("options")?.as_str()?);
                match l {
                    Lang::Shell | Lang::Node => format!("new RegExp({p}, {f})"),
                    Lang::Python => format!("Regex({p}, {f})"),
                    Lang::Java | Lang::CSharp => format!("new BsonRegularExpression({p}, {f})"),
                    Lang::Go => format!("bson.Regex{{Pattern: {p}, Options: {f}}}"),
                    Lang::Rust => format!("(Regex {{ pattern: {p}.into(), options: {f}.into() }})"),
                    Lang::Php => format!("new MongoDB\\BSON\\Regex({p}, {f})"),
                    Lang::Ruby => format!("BSON::Regexp::Raw.new({p}, {f})"),
                }
            }
            ("$binary" | "$timestamp" | "$minKey" | "$maxKey", _) if l == Lang::Shell => {
                mongo::to_shell(&Value::Object(o.clone()))
            }
            _ => return None,
        })
    }

    fn value(&mut self, v: &Value, indent: usize) -> String {
        match v {
            Value::Null => self.null().into(),
            Value::Bool(b) => self.boolean(*b).into(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => self.string(s),
            Value::Array(a) => {
                let items: Vec<String> = a.iter().map(|x| self.value(x, indent + 1)).collect();
                let (open, close) = match self.lang {
                    Lang::Java => ("Arrays.asList(", ")"),
                    Lang::CSharp => ("new BsonArray { ", " }"),
                    Lang::Go => ("bson.A{", "}"),
                    _ => ("[", "]"),
                };
                if items.is_empty() {
                    return match self.lang {
                        Lang::Java => "Arrays.asList()".into(),
                        Lang::CSharp => "new BsonArray()".into(),
                        _ => format!("{}{}", open.trim_end(), close.trim_start()),
                    };
                }
                self.list(open, close, items, indent)
            }
            Value::Object(o) => {
                if let Some(s) = self.special(o) {
                    return s;
                }
                if self.lang == Lang::Java {
                    return self.java_document(o, indent);
                }
                let entries: Vec<String> = o
                    .iter()
                    .map(|(k, x)| {
                        let key = self.key(k);
                        let val = self.value(x, indent + 1);
                        match self.lang {
                            Lang::Php | Lang::Ruby => format!("{key} => {val}"),
                            Lang::CSharp => format!("{{ {key}, {val} }}"),
                            Lang::Go => format!("{{{key}, {val}}}"),
                            _ => format!("{key}: {val}"),
                        }
                    })
                    .collect();
                let (open, close) = match self.lang {
                    Lang::Php => ("[", "]"),
                    Lang::CSharp => ("new BsonDocument { ", " }"),
                    Lang::Go => ("bson.D{", "}"),
                    _ => ("{ ", " }"),
                };
                if entries.is_empty() {
                    return match self.lang {
                        Lang::Php => "new stdClass()".into(),
                        Lang::CSharp => "new BsonDocument()".into(),
                        Lang::Go => "bson.D{}".into(),
                        _ => "{}".into(),
                    };
                }
                self.list(open, close, entries, indent)
            }
        }
    }

    fn key(&self, k: &str) -> String {
        let plain = !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            && !k.starts_with(|c: char| c.is_ascii_digit());
        match self.lang {
            Lang::Shell | Lang::Node if plain => k.to_string(),
            _ => self.string(k),
        }
    }

    /// Items between `open` and `close`: on one line when short, else one per line.
    fn list(&self, open: &str, close: &str, items: Vec<String>, indent: usize) -> String {
        let one = items.join(", ");
        if one.len() + indent * 2 <= 72 && !one.contains('\n') {
            return format!("{open}{one}{close}");
        }
        let pad = self.pad(indent + 1);
        let mut s = format!("{}\n", open.trim_end());
        let trailing = !matches!(self.lang, Lang::Java | Lang::Python | Lang::Ruby | Lang::Shell | Lang::Node);
        for (i, item) in items.iter().enumerate() {
            s.push_str(&pad);
            s.push_str(item);
            if i + 1 < items.len() || trailing {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str(&self.pad(indent));
        s.push_str(close.trim_start());
        s
    }

    fn java_document(&mut self, o: &Map<String, Value>, indent: usize) -> String {
        if o.is_empty() {
            return "new Document()".into();
        }
        let parts: Vec<(String, String)> = o.iter().map(|(k, x)| (self.string(k), self.value(x, indent + 1))).collect();
        let mut s = format!("new Document({}, {})", parts[0].0, parts[0].1);
        let one_line = parts.iter().map(|(k, v)| k.len() + v.len() + 12).sum::<usize>() + indent * 2 <= 72
            && parts.iter().all(|(_, v)| !v.contains('\n'));
        for (k, v) in &parts[1..] {
            if one_line {
                s.push_str(&format!(".append({k}, {v})"));
            } else {
                s.push_str(&format!("\n{}.append({k}, {v})", self.pad(indent + 1)));
            }
        }
        s
    }
}

/// Indents every line after the first by `n` spaces.
fn indent_rest(s: &str, n: usize) -> String {
    s.replace('\n', &format!("\n{}", " ".repeat(n)))
}

/// The pipeline `stages` (Extended JSON) run on `collection` of `database`,
/// as a complete program in `lang` connecting to `uri`.
pub fn export(lang: Lang, uri: &str, database: &str, collection: &str, stages: &[Value]) -> String {
    let mut w = Writer { lang, uses: Uses::default() };
    let pipeline = if lang == Lang::Rust {
        let docs: Vec<String> = stages.iter().map(|s| format!("doc! {}", w.value(s, 1))).collect();
        w.list("vec![", "]", docs, 0)
    } else if lang == Lang::Go {
        let docs: Vec<String> = stages.iter().map(|s| w.value(s, 1)).collect();
        if docs.is_empty() { "mongo.Pipeline{}".into() } else { w.list("mongo.Pipeline{", "}", docs, 0) }
    } else if lang == Lang::CSharp {
        let docs: Vec<String> = stages.iter().map(|s| w.value(s, 1)).collect();
        if docs.is_empty() { "new BsonDocument[] { }".into() } else { w.list("new BsonDocument[] { ", " }", docs, 0) }
    } else {
        w.value(&Value::Array(stages.to_vec()), 0)
    };
    let u = &w.uses;
    let s = |x: &str| w.string(x);
    match lang {
        Lang::Shell => {
            format!("use({});\n\n{}.aggregate({pipeline});\n", s(database), mongo::collection_ref(collection))
        }
        Lang::Python => {
            let mut imports = vec!["from pymongo import MongoClient".to_string()];
            let mut bson: Vec<&str> = Vec::new();
            if u.object_id {
                bson.push("ObjectId");
            }
            if u.long {
                bson.push("Int64");
            }
            if u.decimal {
                bson.push("Decimal128");
            }
            if u.regex {
                bson.push("Regex");
            }
            if !bson.is_empty() {
                imports.insert(0, format!("from bson import {}", bson.join(", ")));
            }
            if u.date {
                imports.insert(0, "from datetime import datetime, timezone".into());
            }
            format!(
                "{}\n\nclient = MongoClient({})\ncollection = client[{}][{}]\n\npipeline = {pipeline}\n\nfor doc in collection.aggregate(pipeline):\n    print(doc)\n",
                imports.join("\n"),
                s(uri),
                s(database),
                s(collection),
            )
        }
        Lang::Node => {
            let mut names = vec!["MongoClient"];
            if u.object_id {
                names.push("ObjectId");
            }
            if u.long {
                names.push("Long");
            }
            if u.decimal {
                names.push("Decimal128");
            }
            format!(
                "const {{ {} }} = require(\"mongodb\");\n\nasync function main() {{\n  const client = new MongoClient({});\n  try {{\n    const collection = client.db({}).collection({});\n    const pipeline = {};\n    for await (const doc of collection.aggregate(pipeline)) {{\n      console.log(doc);\n    }}\n  }} finally {{\n    await client.close();\n  }}\n}}\n\nmain().catch(console.error);\n",
                names.join(", "),
                s(uri),
                s(database),
                s(collection),
                indent_rest(&pipeline, 4),
            )
        }
        Lang::Java => {
            let mut imports = vec![
                "import com.mongodb.client.MongoClient;",
                "import com.mongodb.client.MongoClients;",
                "import com.mongodb.client.MongoCollection;",
                "import org.bson.Document;",
                "import java.util.Arrays;",
                "import java.util.List;",
            ];
            if u.object_id {
                imports.push("import org.bson.types.ObjectId;");
            }
            if u.decimal {
                imports.push("import org.bson.types.Decimal128;");
            }
            if u.regex {
                imports.push("import org.bson.BsonRegularExpression;");
            }
            if u.date {
                imports.push("import java.time.Instant;");
                imports.push("import java.util.Date;");
            }
            format!(
                "{}\n\npublic class Aggregate {{\n    public static void main(String[] args) {{\n        try (MongoClient client = MongoClients.create({})) {{\n            MongoCollection<Document> collection = client.getDatabase({}).getCollection({});\n            List<Document> pipeline = {};\n            for (Document doc : collection.aggregate(pipeline)) {{\n                System.out.println(doc.toJson());\n            }}\n        }}\n    }}\n}}\n",
                imports.join("\n"),
                s(uri),
                s(database),
                s(collection),
                indent_rest(&pipeline, 12),
            )
        }
        Lang::CSharp => format!(
            "using MongoDB.Bson;\nusing MongoDB.Driver;\n\nvar client = new MongoClient({});\nvar collection = client.GetDatabase({}).GetCollection<BsonDocument>({});\n\nvar pipeline = {pipeline};\n\nforeach (var doc in collection.Aggregate<BsonDocument>(pipeline).ToList())\n{{\n    Console.WriteLine(doc);\n}}\n",
            s(uri),
            s(database),
            s(collection),
        ),
        Lang::Go => {
            let mut imports = vec!["\"context\"", "\"fmt\"", "\"log\""];
            if u.date {
                imports.push("\"time\"");
            }
            imports.push("");
            imports.push("\"go.mongodb.org/mongo-driver/v2/bson\"");
            imports.push("\"go.mongodb.org/mongo-driver/v2/mongo\"");
            imports.push("\"go.mongodb.org/mongo-driver/v2/mongo/options\"");
            let mut helpers = String::new();
            if u.object_id {
                helpers.push_str("\nfunc oid(hex string) bson.ObjectID {\n\tid, err := bson.ObjectIDFromHex(hex)\n\tif err != nil {\n\t\tlog.Fatal(err)\n\t}\n\treturn id\n}\n");
            }
            if u.decimal {
                helpers.push_str("\nfunc dec(s string) bson.Decimal128 {\n\td, err := bson.ParseDecimal128(s)\n\tif err != nil {\n\t\tlog.Fatal(err)\n\t}\n\treturn d\n}\n");
            }
            let imports: Vec<String> =
                imports.iter().map(|i| if i.is_empty() { String::new() } else { format!("\t{i}") }).collect();
            format!(
                "package main\n\nimport (\n{}\n)\n{helpers}\nfunc main() {{\n\tclient, err := mongo.Connect(options.Client().ApplyURI({}))\n\tif err != nil {{\n\t\tlog.Fatal(err)\n\t}}\n\tdefer client.Disconnect(context.Background())\n\n\tcollection := client.Database({}).Collection({})\n\tpipeline := {}\n\n\tcursor, err := collection.Aggregate(context.Background(), pipeline)\n\tif err != nil {{\n\t\tlog.Fatal(err)\n\t}}\n\tvar results []bson.M\n\tif err := cursor.All(context.Background(), &results); err != nil {{\n\t\tlog.Fatal(err)\n\t}}\n\tfor _, doc := range results {{\n\t\tfmt.Println(doc)\n\t}}\n}}\n",
                imports.join("\n"),
                s(uri),
                s(database),
                s(collection),
                indent_rest(&pipeline, 1).replace("\n ", "\n\t").replace("  ", "\t"),
            )
        }
        Lang::Rust => {
            let mut names = vec!["doc", "Document"];
            if u.object_id {
                names.push("oid::ObjectId");
            }
            if u.date {
                names.push("DateTime");
            }
            if u.decimal {
                names.push("Decimal128");
            }
            if u.regex {
                names.push("Regex");
            }
            format!(
                "use futures::TryStreamExt;\nuse mongodb::{{Client, bson::{{{}}}}};\n\n#[tokio::main]\nasync fn main() -> mongodb::error::Result<()> {{\n    let client = Client::with_uri_str({}).await?;\n    let collection = client.database({}).collection::<Document>({});\n    let pipeline = {};\n    let mut cursor = collection.aggregate(pipeline).await?;\n    while let Some(doc) = cursor.try_next().await? {{\n        println!(\"{{doc}}\");\n    }}\n    Ok(())\n}}\n",
                names.join(", "),
                s(uri),
                s(database),
                s(collection),
                indent_rest(&pipeline, 4),
            )
        }
        Lang::Php => format!(
            "<?php\n\nrequire 'vendor/autoload.php';\n\n$client = new MongoDB\\Client({});\n$collection = $client->selectCollection({}, {});\n\n$pipeline = {pipeline};\n\nforeach ($collection->aggregate($pipeline) as $doc) {{\n    echo json_encode($doc), PHP_EOL;\n}}\n",
            s(uri),
            s(database),
            s(collection),
        ),
        Lang::Ruby => format!(
            "require 'mongo'\n\nclient = Mongo::Client.new({}, database: {})\ncollection = client[{}]\n\npipeline = {pipeline}\n\ncollection.aggregate(pipeline).each do |doc|\n  puts doc\nend\n",
            s(uri),
            s(database),
            s(collection),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stages() -> Vec<Stage> {
        vec![
            Stage {
                name: "$match".into(),
                body: "{ city: 'Berlin', _id: ObjectId('65a1b2c3d4e5f60718293a4b') }".into(),
                enabled: true,
            },
            Stage { name: "$sort".into(), body: "{ age: -1 }".into(), enabled: false },
            Stage {
                name: "$group".into(),
                body: "{ _id: '$city', since: { $min: ISODate('2024-05-01T10:00:00Z') }, n: { $sum: NumberLong(1) } }"
                    .into(),
                enabled: true,
            },
        ]
    }

    #[test]
    fn catalog() {
        assert!(OPERATORS.len() >= 200, "{}", OPERATORS.len());
        assert!(STAGES.len() >= 30);
        // Every snippet and template is valid shell syntax.
        for s in STAGES {
            mongo::parse_value(s.template).unwrap_or_else(|e| panic!("{}: {e}", s.name));
        }
        for o in OPERATORS {
            let text = if o.snippet.starts_with('{') || o.snippet.starts_with('"') {
                o.snippet.to_string()
            } else {
                format!("{{ {} }}", o.snippet)
            };
            mongo::parse_value(&text).unwrap_or_else(|e| panic!("{}: {e}", o.name));
        }
        assert_eq!(find_operators("datetostr").iter().map(|o| o.name).collect::<Vec<_>>(), ["$dateToString"]);
        assert!(find_operators("trigonometry").len() >= 15);
    }

    #[test]
    fn pipelines() {
        let s = stages();
        let p = pipeline(&s, 2).unwrap();
        assert_eq!(p.as_array().unwrap().len(), 2, "the disabled stage is left out");
        assert_eq!(p[0]["$match"]["city"], "Berlin");
        assert_eq!(pipeline(&s, 0).unwrap().as_array().unwrap().len(), 1);
        let pv = preview(&s, 0, 20).unwrap().unwrap();
        assert_eq!(pv[1], json!({ "$limit": 20 }));

        let mut bad = s.clone();
        bad[2].body = "{ _id: ".into();
        assert_eq!(pipeline(&bad, 2).unwrap_err().0, 2);
        assert!(pipeline(&bad, 1).is_ok());

        let mut out = s.clone();
        out.push(Stage::new("$out"));
        assert_eq!(preview(&out, 3, 20).unwrap(), None);
        assert!(preview(&out, 2, 20).unwrap().is_some());

        let parsed = stages_from_text("[{ $match: { a: 1 } }, { $limit: 5 }]").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].name.as_str(), parsed[0].body.as_str()), ("$match", "{ \"a\": 1 }"));
        assert_eq!(parsed[1].body, "5");
        assert!(stages_from_text("{ a: 1 }").is_err());
        assert!(stages_from_text("[{ a: 1, b: 2 }]").is_err());
    }

    #[test]
    fn code() {
        let p = pipeline(&stages(), 2).unwrap();
        let p = p.as_array().unwrap();
        let uri = "mongodb://localhost:27017";
        let code = |l| export(l, uri, "shop", "people", p);

        let shell = code(Lang::Shell);
        assert!(shell.starts_with("use(\"shop\");"), "{shell}");
        assert!(shell.contains("ObjectId(\"65a1b2c3d4e5f60718293a4b\")"), "{shell}");
        // The shell's code reads back as the same pipeline.
        let call = shell.lines().skip(2).collect::<Vec<_>>().join("\n");
        let mongo::Shell::Collection { call, .. } = mongo::parse(&call).unwrap() else { panic!() };
        assert_eq!(call.args[0], Value::Array(p.clone()));

        let py = code(Lang::Python);
        assert!(py.contains("from datetime import datetime, timezone"), "{py}");
        assert!(py.contains("from bson import ObjectId, Int64"), "{py}");
        assert!(py.contains("datetime(2024, 5, 1, 10, 0, 0, tzinfo=timezone.utc)"), "{py}");
        assert!(py.contains("\"n\": { \"$sum\": Int64(1) }"), "{py}");

        let node = code(Lang::Node);
        assert!(node.contains("const { MongoClient, ObjectId, Long } = require(\"mongodb\");"), "{node}");
        assert!(node.contains("new Date(\"2024-05-01T10:00:00Z\")"), "{node}");

        let java = code(Lang::Java);
        assert!(java.contains("new Document(\"$match\", new Document(\"city\", \"Berlin\")"), "{java}");
        assert!(java.contains("Date.from(Instant.parse(\"2024-05-01T10:00:00Z\"))"), "{java}");
        assert!(java.contains("1L"), "{java}");

        let cs = code(Lang::CSharp);
        assert!(cs.contains("{ \"$sum\", 1L }"), "{cs}");
        assert!(cs.contains("new DateTime(2024, 5, 1, 10, 0, 0, 0, DateTimeKind.Utc)"), "{cs}");

        let go = code(Lang::Go);
        assert!(go.contains("{\"$match\", bson.D{{\"city\", \"Berlin\"}"), "{go}");
        assert!(go.contains("func oid(hex string) bson.ObjectID"), "{go}");
        assert!(go.contains("time.Date(2024, 5, 1, 10, 0, 0, 0, time.UTC)"), "{go}");

        let rs = code(Lang::Rust);
        assert!(rs.contains("doc! {\n            \"$match\": {"), "{rs}");
        assert!(rs.contains("(ObjectId::parse_str(\"65a1b2c3d4e5f60718293a4b\").unwrap())"), "{rs}");
        assert!(rs.contains("bson::{doc, Document, oid::ObjectId, DateTime}"), "{rs}");

        let php = code(Lang::Php);
        assert!(php.contains("'$match' => [") && php.contains("'city' => 'Berlin',"), "{php}");
        assert!(php.contains("new MongoDB\\BSON\\ObjectId('65a1b2c3d4e5f60718293a4b')"), "{php}");

        let rb = code(Lang::Ruby);
        assert!(rb.contains("'$match' => {") && rb.contains("'city' => 'Berlin',"), "{rb}");
        assert!(rb.contains("Time.utc(2024, 5, 1, 10, 0, 0)"), "{rb}");

        // An empty document and array in every language.
        let empty = [json!({ "$project": {}, }), json!({ "$unset": [] })];
        for l in Lang::ALL {
            let c = export(l, uri, "d", "c", &empty);
            assert!(!c.is_empty());
        }
        assert!(export(Lang::Php, uri, "d", "c", &empty).contains("new stdClass()"));
    }

    #[test]
    fn dates() {
        assert_eq!(date_parts("2024-05-01T10:00:00Z"), Some([2024, 5, 1, 10, 0, 0, 0]));
        assert_eq!(date_parts("2024-05-01T10:00:00.25Z"), Some([2024, 5, 1, 10, 0, 0, 250]));
    }
}
