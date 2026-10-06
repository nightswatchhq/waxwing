//! Restore a dump faster than `graphman restore` does, with the indexes the
//! dump recorded. graphman still creates the deployment, from a skeleton
//! dump with no rows and no head, under a config that assigns it to a node
//! that does not exist. waxwing then loads the rows with `COPY`, builds the
//! indexes, sets the head, and hands the deployment to its real node.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{DataType, Int32Type, Int64Type, TimeUnit, TimestampMicrosecondType};
use inflector::Inflector;
use postgres::Client;

use super::diff::{Clamps, VidOrder, batches, int32, vids};
use super::indexes::plan_in;
use super::{DumpTable, check_relative, read_metadata};

/// The node a deployment is assigned to while waxwing loads it.
pub const PARKED_NODE: &str = "waxwing_parked";
const DATA_SOURCES_TABLE: &str = "data_sources$";

pub struct RestoreOptions {
    /// The shard database the deployment is restored into.
    pub db: String,
    /// The primary database, where graph-node keeps its catalogue of
    /// deployments; the same as `db` with a single database.
    pub primary_db: Option<String>,
    /// How to run graphman, e.g. `graphman` or `docker exec -i node graphman`.
    pub graphman: Vec<String>,
    /// The operator's graphman config.
    pub config: PathBuf,
    /// An empty directory for the skeleton dump and the parked config.
    pub work: PathBuf,
    /// Where graphman sees `work`, if not at the same path.
    pub work_as: Option<String>,
    pub name: String,
    pub shard: String,
    /// The node that indexes the deployment once it is restored.
    pub node: String,
}

/// Table names as graph-node derives them from entity types.
fn sql_name(object: &str) -> String {
    match object {
        "Poi$" => "poi2$".to_string(),
        DATA_SOURCES_TABLE => object.to_string(),
        _ => object.to_snake_case(),
    }
}

/// The dump's metadata with every row and the head taken out: what graphman
/// needs to create the deployment, its tables and its metadata.
fn skeleton(raw: &serde_json::Value) -> Result<serde_json::Value> {
    let mut skeleton = raw.clone();
    let tables = skeleton["tables"]
        .as_object_mut()
        .context("metadata.json has no tables")?;
    for table in tables.values_mut() {
        table["chunks"] = serde_json::json!([]);
        table["clamps"] = serde_json::json!([]);
    }
    skeleton["head_block"] = serde_json::Value::Null;
    Ok(skeleton)
}

/// The operator's config, with a first deployment rule sending `name` to a
/// node that does not exist.
fn parked_config(config: &str, name: &str, shard: &str) -> Result<String> {
    let mut config: toml::Table = config.parse().context("parsing the graphman config")?;
    let mut rule = toml::Table::new();
    let mut matcher = toml::Table::new();
    matcher.insert("name".into(), format!("^{}$", regex_escape(name)).into());
    rule.insert("match".into(), matcher.into());
    rule.insert("shard".into(), shard.into());
    rule.insert(
        "indexers".into(),
        vec![toml::Value::from(PARKED_NODE)].into(),
    );

    let deployment = config
        .entry("deployment")
        .or_insert_with(|| toml::Table::new().into())
        .as_table_mut()
        .context("[deployment] in the graphman config is not a table")?;
    let rules = deployment
        .entry("rule")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .context("deployment.rule in the graphman config is not an array")?;
    rules.insert(0, rule.into());
    Ok(toml::to_string(&config)?)
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn graphman(options: &RestoreOptions, config: &str, args: &[&str]) -> Result<()> {
    let (program, prefix) = options
        .graphman
        .split_first()
        .context("no graphman command")?;
    let status = Command::new(program)
        .args(prefix)
        .args(["--config", config])
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("graphman {} failed: {status}", args.join(" "));
    }
    Ok(())
}

/// A value as Postgres reads it in a text-format `COPY`, or `None` for null.
fn literal(array: &dyn Array, i: usize) -> Result<Option<String>> {
    if array.is_null(i) {
        return Ok(None);
    }
    let value = match array.data_type() {
        DataType::Boolean => match array.as_boolean().value(i) {
            true => "t".to_string(),
            false => "f".to_string(),
        },
        DataType::Int32 => array.as_primitive::<Int32Type>().value(i).to_string(),
        DataType::Int64 => array.as_primitive::<Int64Type>().value(i).to_string(),
        DataType::Binary => format!("\\x{}", hex::encode(array.as_binary::<i32>().value(i))),
        DataType::Utf8 => array.as_string::<i32>().value(i).to_string(),
        DataType::Timestamp(TimeUnit::Microsecond, None) => {
            let micros = array.as_primitive::<TimestampMicrosecondType>().value(i);
            let time = arrow::temporal_conversions::timestamp_us_to_datetime(micros)
                .context("timestamp out of range")?;
            format!("{}+00", time.format("%Y-%m-%d %H:%M:%S%.6f"))
        }
        DataType::List(_) => {
            let items = array.as_list::<i32>().value(i);
            let mut out = String::from("{");
            for j in 0..items.len() {
                if j > 0 {
                    out.push(',');
                }
                match literal(items.as_ref(), j)? {
                    None => out.push_str("NULL"),
                    Some(item) => {
                        out.push('"');
                        out.push_str(&item.replace('\\', "\\\\").replace('"', "\\\""));
                        out.push('"');
                    }
                }
            }
            out.push('}');
            out
        }
        other => bail!("cannot restore a column of type {other}"),
    };
    Ok(Some(value))
}

fn copy_field(out: &mut Vec<u8>, value: Option<&str>) {
    let Some(value) = value else {
        out.extend_from_slice(b"\\N");
        return;
    };
    for byte in value.bytes() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(byte),
        }
    }
}

/// Load one table's chunks into `nsp.table`, clamp files folded in.
/// A `@fulltext` search: a tsvector column on the table of the entity it
/// includes, which graph-node computes on insert and a dump leaves out.
#[derive(Debug, PartialEq, Eq)]
struct Fulltext {
    table: String,
    column: String,
    /// A text search configuration, quoted, as graph-node writes it.
    language: &'static str,
    fields: Vec<String>,
}

impl Fulltext {
    /// The expression graph-node inserts: one `to_tsvector` per non-null
    /// field, concatenated. A null field gives an empty tsvector, which
    /// concatenation leaves alone.
    fn expression(&self) -> String {
        let parts: Vec<String> = self
            .fields
            .iter()
            .map(|f| format!("to_tsvector({}, coalesce(\"{f}\", ''))", self.language))
            .collect();
        match parts.is_empty() {
            true => "''::tsvector".to_string(),
            false => format!("({})", parts.join(" || ")),
        }
    }
}

fn language(code: &str) -> Result<&'static str> {
    // graph-node's spellings, `portugese` included.
    Ok(match code {
        "simple" => "'simple'",
        "da" => "'danish'",
        "nl" => "'dutch'",
        "en" => "'english'",
        "fi" => "'finnish'",
        "fr" => "'french'",
        "de" => "'german'",
        "hu" => "'hungarian'",
        "it" => "'italian'",
        "no" => "'norwegian'",
        "pt" => "'portugese'",
        "ro" => "'romanian'",
        "ru" => "'russian'",
        "es" => "'spanish'",
        "sv" => "'swedish'",
        "tr" => "'turkish'",
        other => bail!("unknown fulltext language {other}"),
    })
}

/// The `@fulltext` directives on `_Schema_`.
fn fulltexts(schema: &str) -> Result<Vec<Fulltext>> {
    use graphql_parser::schema::{Definition, TypeDefinition, Value};

    let document = graphql_parser::parse_schema::<String>(schema)
        .map_err(|e| anyhow::anyhow!("parsing schema.graphql: {e}"))?;
    let mut out = Vec::new();
    for definition in &document.definitions {
        let Definition::TypeDefinition(TypeDefinition::Object(object)) = definition else {
            continue;
        };
        if object.name != "_Schema_" {
            continue;
        }
        for directive in object.directives.iter().filter(|d| d.name == "fulltext") {
            let arg = |name: &str| {
                directive
                    .arguments
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
                    .with_context(|| format!("@fulltext without {name}"))
            };
            let Value::String(name) = arg("name")? else {
                bail!("@fulltext name is not a string");
            };
            let Value::Enum(code) = arg("language")? else {
                bail!("@fulltext language is not an enum");
            };
            // graph-node allows one included entity.
            let Value::List(include) = arg("include")? else {
                bail!("@fulltext include is not a list");
            };
            let Some(Value::Object(entity)) = include.first() else {
                bail!("@fulltext {name} includes no entity");
            };
            let (Some(Value::String(entity_name)), Some(Value::List(fields))) =
                (entity.get("entity"), entity.get("fields"))
            else {
                bail!("@fulltext {name} has a malformed include");
            };
            let mut columns = Vec::new();
            for field in fields {
                let Value::Object(field) = field else {
                    bail!("@fulltext {name} has a malformed field");
                };
                let Some(Value::String(field)) = field.get("name") else {
                    bail!("@fulltext {name} has a field without a name");
                };
                columns.push(field.to_snake_case());
            }
            out.push(Fulltext {
                table: sql_name(entity_name),
                column: name.to_snake_case(),
                language: language(code)?,
                fields: columns,
            });
        }
    }
    Ok(out)
}

fn load_table(
    db: &mut Client,
    dir: &Path,
    nsp: &str,
    table: &str,
    dump: &DumpTable,
    fulltext: &[&Fulltext],
    after: i64,
) -> Result<usize> {
    let mut clamps = Clamps::open(dir, dump)?;
    let mut order = VidOrder::default();
    let mut rows = 0;

    // With fulltext columns, rows go through a staging table so the
    // tsvectors are computed as they are inserted, not by a second pass.
    let target = match fulltext.is_empty() {
        true => format!("\"{nsp}\".\"{table}\""),
        false => {
            let mut sql = format!(
                "create temp table waxwing_stage (like \"{nsp}\".\"{table}\" including defaults)"
            );
            for f in fulltext {
                sql.push_str(&format!(
                    "; alter table waxwing_stage drop column \"{}\"",
                    f.column
                ));
            }
            db.batch_execute(&sql)?;
            "waxwing_stage".to_string()
        }
    };

    for chunk in &dump.chunks {
        check_relative(&chunk.file)?;
        for batch in batches(&dir.join(&chunk.file))? {
            let batch: RecordBatch = batch?;
            let schema = batch.schema();
            let vids = vids(&batch, &chunk.file)?;
            let range = schema.column_with_name("block_range_start").is_some();
            let (starts, ends) = match range {
                true => (
                    Some(int32(&batch, "block_range_start")?),
                    Some(int32(&batch, "block_range_end")?),
                ),
                false => (None, None),
            };

            let mut columns = Vec::new();
            for field in schema.fields() {
                match field.name().as_str() {
                    "block_range_start" => columns.push("\"block_range\"".to_string()),
                    "block_range_end" => {}
                    name => columns.push(format!("\"{}\"", name.replace('"', "\"\""))),
                }
            }
            let sql = format!("copy {target} ({}) from stdin", columns.join(", "));

            let mut out = Vec::new();
            let mut copied = 0;
            for i in 0..batch.num_rows() {
                let vid = vids.value(i);
                order.check(vid, &chunk.file)?;
                let clamp = clamps.end(vid)?;
                // Loaded before an interruption. Each batch commits whole.
                if vid <= after {
                    continue;
                }
                copied += 1;
                let mut first = true;
                for (field, column) in schema.fields().iter().zip(batch.columns()) {
                    let value = match field.name().as_str() {
                        "block_range_end" => continue,
                        "block_range_start" => {
                            let (starts, ends) = (starts.unwrap(), ends.unwrap());
                            let end = clamp.or_else(|| ends.is_valid(i).then(|| ends.value(i)));
                            Some(match end {
                                Some(end) => format!("[{},{end})", starts.value(i)),
                                None => format!("[{},)", starts.value(i)),
                            })
                        }
                        _ => literal(column.as_ref(), i)?,
                    };
                    if !first {
                        out.push(b'\t');
                    }
                    first = false;
                    copy_field(&mut out, value.as_deref());
                }
                out.push(b'\n');
            }

            if copied == 0 {
                continue;
            }
            let mut writer = db.copy_in(&sql)?;
            writer.write_all(&out)?;
            writer
                .finish()
                .with_context(|| format!("loading {}", chunk.file))?;
            if !fulltext.is_empty() {
                let names: Vec<String> = fulltext
                    .iter()
                    .map(|f| format!("\"{}\"", f.column))
                    .collect();
                let exprs: Vec<String> = fulltext.iter().map(|f| f.expression()).collect();
                let columns = columns.join(", ");
                db.batch_execute(&format!(
                    "insert into \"{nsp}\".\"{table}\" ({columns}, {}) select {columns}, {} from waxwing_stage; truncate waxwing_stage",
                    names.join(", "),
                    exprs.join(", ")
                ))?;
            }
            rows += copied;
        }
    }
    if !fulltext.is_empty() {
        db.batch_execute("drop table waxwing_stage")?;
    }
    Ok(rows)
}

/// Restore the dump in `dir`; `progress` hears what is happening.
pub fn restore(dir: &Path, options: &RestoreOptions, mut progress: impl FnMut(&str)) -> Result<()> {
    let metadata = read_metadata(dir)?;
    let schema =
        fs::read_to_string(dir.join("schema.graphql")).context("reading schema.graphql")?;
    let fulltexts = fulltexts(&schema)?;
    let Some(head) = &metadata.head_block else {
        bail!("dump has no head block");
    };
    let mut db = postgres::Client::connect(&options.db, postgres::NoTls)
        .context("connecting to the database")?;
    let mut primary = match &options.primary_db {
        Some(url) => Some(
            postgres::Client::connect(url, postgres::NoTls)
                .context("connecting to the primary database")?,
        ),
        None => None,
    };

    let raw: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("metadata.json"))?)?;
    let work_as = match &options.work_as {
        Some(path) => path.trim_end_matches('/').to_string(),
        None => {
            fs::create_dir_all(&options.work)?;
            fs::canonicalize(&options.work)?.display().to_string()
        }
    };
    let graphman_config = format!("{work_as}/graphman.toml");

    let find = |db: &mut Client| -> Result<Option<(i32, String, Option<String>)>> {
        let row = db.query_opt(
            "select s.id, s.name::text, a.node_id
               from public.deployment_schemas s
               left join subgraphs.subgraph_deployment_assignment a on a.id = s.id
              where s.subgraph = $1 and s.shard = $2",
            &[&metadata.deployment, &options.shard],
        )?;
        Ok(row.map(|row| (row.get(0), row.get(1), row.get(2))))
    };
    let existing = find(primary.as_mut().unwrap_or(&mut db))?;
    let resuming = existing.is_some();
    if resuming {
        // Only a restore of ours, interrupted, is parked with this work dir.
        if !options.work.join("graphman.toml").exists() {
            bail!(
                "{} already exists in shard {}, and {} holds no restore of waxwing's to resume",
                metadata.deployment,
                options.shard,
                options.work.display()
            );
        }
    } else {
        if options.work.exists() && fs::read_dir(&options.work)?.next().is_some() {
            bail!("{} is not empty", options.work.display());
        }
        // The skeleton and the parked config, where graphman can read them.
        let skeleton_dir = options.work.join("skeleton");
        fs::create_dir_all(&skeleton_dir)?;
        fs::write(
            skeleton_dir.join("metadata.json"),
            serde_json::to_vec_pretty(&skeleton(&raw)?)?,
        )?;
        for file in ["schema.graphql", "subgraph.yaml"] {
            if dir.join(file).exists() {
                fs::copy(dir.join(file), skeleton_dir.join(file))?;
            }
        }
        let config = fs::read_to_string(&options.config)
            .with_context(|| format!("reading {}", options.config.display()))?;
        fs::write(
            options.work.join("graphman.toml"),
            parked_config(&config, &options.name, &options.shard)?,
        )?;

        progress("creating the deployment with graphman, parked");
        graphman(
            options,
            &graphman_config,
            &[
                "restore",
                &format!("{work_as}/skeleton"),
                "--name",
                &options.name,
                "--shard",
                &options.shard,
            ],
        )?;
    }

    let Some((site, nsp, node)) = find(primary.as_mut().unwrap_or(&mut db))? else {
        bail!(
            "graphman did not create {} in shard {}",
            metadata.deployment,
            options.shard
        );
    };
    if node.as_deref() != Some(PARKED_NODE) {
        bail!(
            "{nsp} is assigned to {node:?}, not {PARKED_NODE}, and may be indexing: \
             it is not a restore of waxwing's. Drop it with `graphman drop {nsp}`"
        );
    }
    let loaded: Option<i32> = db
        .query_one(
            "select block_number from subgraphs.head where id = $1",
            &[&site],
        )?
        .get(0);
    if loaded.is_some() {
        progress(&format!("{nsp} is already loaded"));
        progress(&format!("handing {nsp} to {}", options.node));
        return graphman(
            options,
            &graphman_config,
            &["reassign", &nsp, &options.node],
        );
    }
    if resuming {
        progress(&format!("resuming the restore into {nsp}"));
    }

    let mut tables = BTreeMap::new();
    for (object, table) in &metadata.tables {
        let name = sql_name(object);
        let exists: bool = db
            .query_one(
                "select exists (select 1 from pg_tables where schemaname = $1 and tablename = $2)",
                &[&nsp, &name],
            )?
            .get(0);
        if !exists {
            bail!("{object} has no table {nsp}.{name}");
        }
        tables.insert(name, table);
    }
    // graphman built the default indexes; only constraints stay for the load.
    let entity_tables: Vec<&String> = metadata.indexes.keys().collect();
    let live: Vec<(String, String)> = db
        .query(
            "select t.relname::text, i.relname::text
               from pg_index x
               join pg_class i on i.oid = x.indexrelid
               join pg_class t on t.oid = x.indrelid
               join pg_namespace n on n.oid = t.relnamespace
              where n.nspname = $1
                and not exists (select 1 from pg_constraint k where k.conindid = i.oid)",
            &[&nsp],
        )?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    for (table, index) in &live {
        if entity_tables.contains(&table) {
            db.batch_execute(&format!("drop index \"{nsp}\".\"{index}\""))?;
        }
    }

    for (name, table) in &tables {
        let after: i64 = db
            .query_one(
                &format!("select coalesce(max(vid), -1)::int8 from \"{nsp}\".\"{name}\""),
                &[],
            )?
            .get(0);
        match after {
            -1 => progress(&format!("loading {name}")),
            _ => progress(&format!("loading {name} after vid {after}")),
        }
        let fulltext: Vec<&Fulltext> = fulltexts.iter().filter(|f| &f.table == name).collect();
        let rows = load_table(&mut db, dir, &nsp, name, table, &fulltext, after)
            .with_context(|| format!("loading {name}"))?;
        progress(&format!("loaded {name}: {rows} rows"));
    }

    let plan = plan_in(&mut db, &nsp, &metadata.indexes, false)?;
    for sql in &plan.create {
        progress(sql);
        db.batch_execute(sql)
            .with_context(|| format!("running `{sql}`"))?;
    }
    for name in tables.keys() {
        db.batch_execute(&format!("analyze \"{nsp}\".\"{name}\""))?;
    }

    // What graphman's finalize would have set had the skeleton had a head.
    let hash = hex::decode(head.hash.trim_start_matches("0x")).context("head block hash")?;
    let entity_count = raw["entity_count"].as_i64().unwrap_or(0);
    db.execute(
        "update subgraphs.head set block_number = $2, block_hash = $3, entity_count = $4 where id = $1",
        &[&site, &head.number, &hash, &entity_count],
    )
    .context("setting the head")?;
    db.execute(
        "update subgraphs.deployment set postponed_indexes_created = true where id = $1",
        &[&site],
    )?;

    progress(&format!("handing {nsp} to {}", options.node));
    graphman(
        options,
        &graphman_config,
        &["reassign", &nsp, &options.node],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{BinaryArray, ListArray, StringArray, TimestampMicrosecondArray};
    use arrow::datatypes::Int32Type as I32;

    #[test]
    fn table_names_follow_graph_node() {
        assert_eq!(sql_name("PingEvent"), "ping_event");
        assert_eq!(sql_name("Poi$"), "poi2$");
        assert_eq!(sql_name("data_sources$"), "data_sources$");
        assert_eq!(sql_name("Stats"), "stats");
    }

    #[test]
    fn literals_and_copy_escapes() {
        let bytes = BinaryArray::from(vec![Some(&[0u8, 255][..]), None]);
        assert_eq!(literal(&bytes, 0).unwrap().as_deref(), Some("\\x00ff"));
        assert_eq!(literal(&bytes, 1).unwrap(), None);

        let time = TimestampMicrosecondArray::from(vec![1_700_000_000_123_456]);
        assert_eq!(
            literal(&time, 0).unwrap().as_deref(),
            Some("2023-11-14 22:13:20.123456+00")
        );

        let list = ListArray::from_iter_primitive::<I32, _, _>(vec![Some(vec![Some(1), None])]);
        assert_eq!(literal(&list, 0).unwrap().as_deref(), Some("{\"1\",NULL}"));

        let text = StringArray::from(vec!["a\tb\\c\"d\n"]);
        let mut out = Vec::new();
        copy_field(&mut out, literal(&text, 0).unwrap().as_deref());
        assert_eq!(out, b"a\\tb\\\\c\"d\\n");
    }

    #[test]
    fn fulltext_directives_become_columns_and_expressions() {
        let schema = r#"
type _Schema_
  @fulltext(
    name: "childSearch"
    language: en
    algorithm: rank
    include: [{ entity: "Child", fields: [{ name: "label" }, { name: "noteText" }] }]
  )

type Child @entity { id: Bytes! label: String! noteText: String }
"#;
        let found = fulltexts(schema).unwrap();
        assert_eq!(
            found,
            [Fulltext {
                table: "child".into(),
                column: "child_search".into(),
                language: "'english'",
                fields: vec!["label".into(), "note_text".into()],
            }]
        );
        assert_eq!(
            found[0].expression(),
            "(to_tsvector('english', coalesce(\"label\", '')) || to_tsvector('english', coalesce(\"note_text\", '')))"
        );
        assert!(
            fulltexts("type Child @entity { id: Bytes! }")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_parking_rule_comes_first() {
        let config = r#"
[store.primary]
connection = "postgresql://${PGUSER}@db/graph"

[deployment]
[[deployment.rule]]
shard = "primary"
indexers = ["index_node_0"]
"#;
        let parked: toml::Table = parked_config(config, "org/sub-graph", "primary")
            .unwrap()
            .parse()
            .unwrap();
        let rules = parked["deployment"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["match"]["name"].as_str(), Some("^org/sub-graph$"));
        assert_eq!(rules[0]["indexers"][0].as_str(), Some(PARKED_NODE));
        assert_eq!(rules[1]["indexers"][0].as_str(), Some("index_node_0"));
        assert_eq!(
            parked["store"]["primary"]["connection"].as_str(),
            Some("postgresql://${PGUSER}@db/graph")
        );
    }

    #[test]
    fn the_skeleton_has_no_rows_and_no_head() {
        let raw = serde_json::json!({
            "head_block": { "number": 9, "hash": "aa" },
            "tables": { "Token": { "chunks": [{ "file": "x" }], "clamps": [{ "file": "y" }], "max_vid": 4 } }
        });
        let skeleton = skeleton(&raw).unwrap();
        assert_eq!(skeleton["head_block"], serde_json::Value::Null);
        assert_eq!(skeleton["tables"]["Token"]["chunks"], serde_json::json!([]));
        assert_eq!(skeleton["tables"]["Token"]["clamps"], serde_json::json!([]));
        assert_eq!(skeleton["tables"]["Token"]["max_vid"], 4);
    }
}
