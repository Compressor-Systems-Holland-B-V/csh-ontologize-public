use std::collections::HashMap;

use anyhow::Result;
use sqlx::{FromRow, PgPool};

use super::OCR_CONFIDENCE;

#[derive(Debug, Clone, FromRow)]
pub struct Document {
    pub id: i64,
    pub source: String,
    pub path: String,
    pub sha256: String,
    pub kind: String,
    pub content_id: i64,
    pub chars: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct Chunk {
    pub id: i64,
    pub content_id: i64,
    pub ord: i32,
    pub page_from: Option<i32>,
    pub text: String,
    pub ocr_confidence: Option<f64>,
}

pub async fn documents(pool: &PgPool) -> Result<Vec<Document>> {
    let rows = sqlx::query_as(
        "select d.id, s.name as source, d.path, coalesce(d.sha256, '') as sha256, \
         coalesce(d.doc_type, d.kind, '') as kind, d.content_id, \
         (select coalesce(sum(length(p.text)), 0) from pages p \
         where p.content_id = d.content_id)::bigint as chars from documents d join sources s on s.id = d.source_id \
         where d.status = 'present' and d.content_id is not null order by s.name, d.path",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn texts(pool: &PgPool, contents: &[i64]) -> Result<HashMap<i64, String>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "select content_id, string_agg(text, E'\\n' order by page_no) from pages \
         where content_id = any($1) group by content_id",
    )
    .bind(contents)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn chunks(pool: &PgPool, contents: &[i64]) -> Result<Vec<Chunk>> {
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select k.id, k.content_id, k.ord, k.page_from, k.text, \
         {OCR_CONFIDENCE} as ocr_confidence from chunks k \
         where k.content_id = any($1) order by k.id"
    )))
    .bind(contents)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
