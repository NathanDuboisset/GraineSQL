//! The driver surface Mongo actually needs.
//!
//! Deliberately small: six operations, all of them document-shaped, so nothing
//! here has to pretend to be SQL.

use anyhow::{Context, Result, bail};
use bson::{Bson, Document, doc};
use futures_util::TryStreamExt;
use mongodb::Client;

use crate::db::{Db, Pool};

/// Documents read per collection when profiling an unvalidated collection.
pub const SAMPLE: i64 = 1_000;

fn client(db: &Db) -> Result<(&Client, &str)> {
    match &db.pool {
        Pool::Mongo(client, name) => Ok((client, name.as_str())),
        _ => bail!(
            "internal: a Mongo operation reached a {:?} pool",
            db.engine()
        ),
    }
}

pub fn database_name(db: &Db) -> Result<String> {
    Ok(client(db)?.1.to_string())
}

/// Collection names, excluding views and system collections.
pub async fn list_collections(db: &Db) -> Result<Vec<String>> {
    let (client, name) = client(db)?;
    let mut names: Vec<String> = client
        .database(name)
        .list_collection_names()
        .await
        .with_context(|| format!("listing collections in {name}"))?
        .into_iter()
        .filter(|c| !c.starts_with("system."))
        .collect();
    names.sort();
    Ok(names)
}

/// A collection's `$jsonSchema` validator, when it has one.
pub async fn validator(db: &Db, collection: &str) -> Result<Option<Document>> {
    let (client, name) = client(db)?;
    let mut cursor = client
        .database(name)
        .run_cursor_command(doc! { "listCollections": 1, "filter": { "name": collection } })
        .await
        .with_context(|| format!("reading the definition of {collection}"))?;

    while let Some(entry) = cursor.try_next().await? {
        if let Ok(options) = entry.get_document("options")
            && let Ok(v) = options.get_document("validator")
            && let Ok(schema) = v.get_document("$jsonSchema")
        {
            return Ok(Some(schema.clone()));
        }
    }
    Ok(None)
}

/// Unique indexes, which are real constraints and so real conflict targets.
pub async fn unique_indexes(db: &Db, collection: &str) -> Result<Vec<Vec<String>>> {
    let (client, name) = client(db)?;
    let mut cursor = client
        .database(name)
        .collection::<Document>(collection)
        .list_indexes()
        .await
        .with_context(|| format!("listing indexes of {collection}"))?;

    let mut out = Vec::new();
    while let Some(index) = cursor.try_next().await? {
        if index.options.as_ref().and_then(|o| o.unique) != Some(true) {
            continue;
        }
        let keys: Vec<String> = index.keys.keys().cloned().collect();
        // `_id` is the primary key, recorded separately.
        if !keys.is_empty() && keys != ["_id"] {
            out.push(keys);
        }
    }
    out.sort();
    Ok(out)
}

/// Documents in `_id` order, optionally capped.
///
/// Sorted rather than `$sample`: a randomised sample would make the lock flap
/// and `lock --check` useless in CI.
pub async fn find_sorted(
    db: &Db,
    collection: &str,
    filter: Option<Document>,
    limit: Option<i64>,
) -> Result<Vec<Document>> {
    let (client, name) = client(db)?;
    let coll = client.database(name).collection::<Document>(collection);
    let mut find = coll
        .find(filter.unwrap_or_default())
        .sort(doc! { "_id": 1 });
    if let Some(n) = limit {
        find = find.limit(n);
    }
    find.await
        .with_context(|| format!("reading {collection}"))?
        .try_collect()
        .await
        .with_context(|| format!("reading {collection}"))
}

pub async fn count(db: &Db, collection: &str) -> Result<u64> {
    let (client, name) = client(db)?;
    client
        .database(name)
        .collection::<Document>(collection)
        .count_documents(doc! {})
        .await
        .with_context(|| format!("counting {collection}"))
}

pub async fn delete_all(db: &Db, collection: &str) -> Result<u64> {
    let (client, name) = client(db)?;
    Ok(client
        .database(name)
        .collection::<Document>(collection)
        .delete_many(doc! {})
        .await
        .with_context(|| format!("emptying {collection}"))?
        .deleted_count)
}

/// How a load should place each document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// `replaceOne({_id}, doc, upsert)`.
    Replace,
    /// `insertMany(ordered)`; a duplicate `_id` aborts.
    Insert,
    /// `updateOne({_id}, {$setOnInsert}, upsert)`, which leaves existing
    /// documents alone without swallowing errors that are not duplicates.
    SetOnInsert,
}

/// Write documents, ordered, so a failure names the one that failed.
pub async fn write(db: &Db, collection: &str, docs: &[Document], mode: Write) -> Result<u64> {
    let (client, name) = client(db)?;
    let coll = client.database(name).collection::<Document>(collection);
    let mut affected = 0;

    for (i, doc) in docs.iter().enumerate() {
        let id = doc.get("_id").cloned().unwrap_or(Bson::Null);
        let filter = doc! { "_id": id };
        let n = match mode {
            Write::Replace => {
                coll.replace_one(filter, doc)
                    .upsert(true)
                    .await
                    .with_context(|| format!("{collection}: writing document {}", i + 1))?
                    .modified_count
                    + 1
            }
            Write::Insert => {
                coll.insert_one(doc)
                    .await
                    .with_context(|| format!("{collection}: inserting document {}", i + 1))?;
                1
            }
            Write::SetOnInsert => coll
                .update_one(filter, doc! { "$setOnInsert": doc })
                .upsert(true)
                .await
                .with_context(|| format!("{collection}: inserting document {}", i + 1))?
                .upserted_id
                .is_some() as u64,
        };
        affected += n;
    }
    Ok(affected)
}

/// Whether the server can run a multi-document transaction.
///
/// Only a replica set or mongos can; a standalone `mongod` rejects one, and a
/// silent downgrade would break the promise that a failed load changes nothing.
pub async fn supports_transactions(db: &Db) -> Result<bool> {
    let (client, name) = client(db)?;
    let hello = client
        .database(name)
        .run_command(doc! { "hello": 1 })
        .await
        .context("asking the server whether it is a replica set")?;
    Ok(hello.contains_key("setName") || hello.get_str("msg") == Ok("isdbgrid"))
}
