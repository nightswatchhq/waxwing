//! Bring a restored deployment's indexes into line with the ones its dump
//! recorded. `graphman restore` builds graph-node's defaults instead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use postgres::Client;

use super::read_metadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPlan {
    pub namespace: String,
    /// Statements building the indexes the dump has and the deployment
    /// does not.
    pub create: Vec<String>,
    /// Statements dropping the indexes the deployment has and the dump
    /// does not.
    pub drop: Vec<String>,
}

#[derive(Debug)]
struct LiveIndex {
    table: String,
    name: String,
    /// Backs a primary key, unique or exclusion constraint.
    constraint: bool,
}

/// The name a dumped `create index` statement gives its index, unquoted.
fn index_name(sql: &str) -> Result<&str> {
    let parse = || {
        let (_, rest) = sql.split_once("index ")?;
        let rest = rest.strip_prefix("if not exists ").unwrap_or(rest);
        let (name, _) = rest.split_once(" on ")?;
        Some(name.trim_matches('"'))
    };
    parse().with_context(|| format!("cannot read the index name in `{sql}`"))
}

/// A dumped statement as one to run against namespace `nsp`. graph-node
/// writes `sgd` in place of the source's namespace.
fn in_namespace(sql: &str, nsp: &str) -> Result<String> {
    let Some((head, tail)) = sql.split_once(" on sgd.") else {
        bail!("`{sql}` is not on namespace sgd");
    };
    let head = head.replacen("index ", "index concurrently ", 1);
    Ok(format!("{head} on \"{nsp}\".{tail}"))
}

fn plan(
    nsp: &str,
    dumped: &BTreeMap<String, Vec<String>>,
    live: &[LiveIndex],
) -> Result<IndexPlan> {
    let have: BTreeSet<&str> = live.iter().map(|i| i.name.as_str()).collect();
    let mut wanted = BTreeSet::new();
    let mut create = Vec::new();
    for sql in dumped.values().flatten() {
        let name = index_name(sql)?;
        wanted.insert(name);
        if !have.contains(name) {
            create.push(in_namespace(sql, nsp)?);
        }
    }
    // Only tables the dump lists indexes for: it has none for data_sources$.
    let drop = live
        .iter()
        .filter(|i| dumped.contains_key(&i.table))
        .filter(|i| !i.constraint && !wanted.contains(i.name.as_str()))
        .map(|i| format!("drop index concurrently if exists \"{nsp}\".\"{}\"", i.name))
        .collect();
    Ok(IndexPlan {
        namespace: nsp.to_string(),
        create,
        drop,
    })
}

/// Compare the indexes of the deployment restored from `dir` with the ones
/// the dump recorded. `namespace` picks one copy where the database holds
/// several.
pub fn index_plan(dir: &Path, db: &mut Client, namespace: Option<&str>) -> Result<IndexPlan> {
    let metadata = read_metadata(dir)?;
    if metadata.indexes.is_empty() {
        bail!("the dump records no indexes");
    }

    let copies: Vec<String> = db
        .query(
            "select name from public.deployment_schemas where subgraph = $1 order by name",
            &[&metadata.deployment],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let nsp = match (namespace, copies.as_slice()) {
        (Some(nsp), _) if copies.iter().any(|c| c == nsp) => nsp.to_string(),
        (Some(nsp), _) => bail!("{nsp} is not a copy of {}", metadata.deployment),
        (None, [nsp]) => nsp.clone(),
        (None, []) => bail!("{} is not in this database", metadata.deployment),
        (None, _) => bail!(
            "{} has copies in {}: pick one with --namespace",
            metadata.deployment,
            copies.join(", ")
        ),
    };

    let live: Vec<LiveIndex> = db
        .query(
            "select t.relname::text, i.relname::text,
                    exists (select 1 from pg_constraint k where k.conindid = i.oid)
               from pg_index x
               join pg_class i on i.oid = x.indexrelid
               join pg_class t on t.oid = x.indrelid
               join pg_namespace n on n.oid = t.relnamespace
              where n.nspname = $1",
            &[&nsp],
        )?
        .iter()
        .map(|row| LiveIndex {
            table: row.get(0),
            name: row.get(1),
            constraint: row.get(2),
        })
        .collect();
    plan(&nsp, &metadata.indexes, &live)
}

/// Build the missing indexes, then drop the surplus, without locking out a
/// running node. Finally tell graph-node its postponed indexes exist, or it
/// adds back any the source did not have when the deployment next starts.
pub fn apply(plan: &IndexPlan, db: &mut Client, mut progress: impl FnMut(&str)) -> Result<()> {
    for sql in plan.create.iter().chain(&plan.drop) {
        progress(sql);
        db.batch_execute(sql)
            .with_context(|| format!("running `{sql}`"))?;
    }
    db.execute(
        "update subgraphs.deployment set postponed_indexes_created = true
          where id = (select id from public.deployment_schemas where name = $1)",
        &[&plan.namespace],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(table: &str, name: &str, constraint: bool) -> LiveIndex {
        LiveIndex {
            table: table.into(),
            name: name.into(),
            constraint,
        }
    }

    #[test]
    fn restores_the_dumped_set_and_leaves_constraints_alone() {
        let dumped = BTreeMap::from([
            (
                "child".to_string(),
                vec![
                    r#"create index if not exists attr_1_1_child_pings on sgd.child using btree ("pings")"#.to_string(),
                    r#"create index if not exists manual on sgd.child using btree ("pings", "created_at")"#.to_string(),
                    "create unique index if not exists child_pkey on sgd.child using btree (vid)".to_string(),
                ],
            ),
            (
                "poi2$".to_string(),
                vec![r#"create index if not exists "attr_3_0_poi2$_digest" on sgd."poi2$" using btree ("digest")"#.to_string()],
            ),
        ]);
        let live = [
            live("child", "attr_1_1_child_pings", false),
            live("child", "attr_1_2_child_created_at", false),
            live("child", "child_pkey", true),
            live("child", "child_id_key", true),
            live("poi2$", "attr_3_0_poi2$_digest", false),
            live("data_sources$", "gist_block_range_data_sources$", false),
        ];
        let plan = plan("sgd7", &dumped, &live).unwrap();
        assert_eq!(
            plan.create,
            [
                r#"create index concurrently if not exists manual on "sgd7".child using btree ("pings", "created_at")"#
            ]
        );
        assert_eq!(
            plan.drop,
            [r#"drop index concurrently if exists "sgd7"."attr_1_2_child_created_at""#]
        );
    }

    #[test]
    fn unique_and_quoted_names_are_read() {
        let sql =
            r#"create unique index if not exists "poi2$_pkey" on sgd."poi2$" using btree (vid)"#;
        assert_eq!(index_name(sql).unwrap(), "poi2$_pkey");
        assert_eq!(
            in_namespace(sql, "sgd3").unwrap(),
            r#"create unique index concurrently if not exists "poi2$_pkey" on "sgd3"."poi2$" using btree (vid)"#
        );
    }
}
