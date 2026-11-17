use std::collections::{HashMap, HashSet, VecDeque};
use std::marker::PhantomData;

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use sqlx::PgPool;

use super::job::{self, Handler, Options, Outcome, Runner, Work};
use super::{ModelName, Request};
use crate::config::Config;
use crate::db;
use crate::db::llm::JobParams;

pub(crate) struct Batch<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    pub name: &'a ModelName,
    pub task: &'static str,
    pub version_id: i64,
}

struct Requests<'a, A> {
    task: &'static str,
    job: i64,
    by_ord: HashMap<i32, (String, &'a Request)>,
    answer: PhantomData<A>,
}

impl<A: DeserializeOwned> Handler for Requests<'_, A> {
    fn task(&self) -> &'static str {
        self.task
    }

    fn label(&self) -> String {
        format!("{} job {}", self.task, self.job)
    }

    async fn request(&self, ord: i32) -> Result<Option<(String, Request)>> {
        Ok(self
            .by_ord
            .get(&ord)
            .map(|(key, request)| (key.clone(), (*request).clone())))
    }

    fn accepts(&self, _ord: i32, response: &str) -> Result<(), String> {
        serde_json::from_str::<A>(response)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn answered(&self, _ord: i32, _response: &str) -> Result<()> {
        Ok(())
    }

    async fn report(&self) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn parse<A: DeserializeOwned>(
    answers: &HashMap<String, String>,
    key: &str,
) -> Option<A> {
    answers
        .get(key)
        .and_then(|text| serde_json::from_str(text).ok())
}

pub(crate) async fn run<A: DeserializeOwned>(
    batch: &Batch<'_>,
    requests: &[Request],
    options: &Options,
) -> Result<Option<HashMap<String, String>>> {
    let Batch {
        pool,
        config,
        name,
        task,
        version_id,
    } = *batch;
    let keys: Vec<String> = requests.iter().map(|r| r.key(name)).collect();
    let (cached, invalid) = db::llm::known_keys(pool, &keys).await?;
    let skip: HashSet<&String> = cached.iter().chain(&invalid).collect();
    let mut seen = HashSet::new();
    let todo: Vec<(String, &Request)> = keys
        .iter()
        .zip(requests)
        .filter(|(key, _)| !skip.contains(key) && seen.insert(*key))
        .map(|(key, request)| (key.clone(), request))
        .collect();
    println!(
        "{task}: {} requests, {} from the cache, {} invalid before, {} to send",
        requests.len(),
        cached.len(),
        invalid.len(),
        todo.len()
    );
    if todo.is_empty() {
        return Ok(Some(db::llm::cached(pool, &keys).await?));
    }
    let params = JobParams::default();
    let open = job::reopen(pool, task, name, version_id, &params, options, true).await?;
    let job = match (open, options.dry_run) {
        (Some(open), _) => Some(open),
        (None, true) => None,
        (None, false) => {
            let created = db::llm::create_job(
                pool,
                task,
                name.provider.as_str(),
                &name.model,
                version_id,
                &params,
                &[],
            )
            .await?;
            println!("{task} job {}: new", created.id);
            Some(created)
        }
    };
    let job_id = job.as_ref().map_or(0, |j| j.id);
    let mut lock = pool.acquire().await?;
    if job_id > 0 && !db::llm::lock(&mut lock, job_id).await? {
        bail!("{task} job {job_id} is running in another process");
    }
    let ords: Vec<i32> = if job_id > 0 {
        let todo_keys: Vec<String> = todo.iter().map(|(key, _)| key.clone()).collect();
        db::llm::replace_key_items(pool, job_id, &todo_keys).await?
    } else {
        (1..=todo.len() as i32).collect()
    };
    let queue: VecDeque<Work> = ords
        .iter()
        .zip(&todo)
        .map(|(ord, (_, request))| Work {
            ord: *ord,
            chars: request.chars(),
        })
        .collect();
    let handler = Requests::<A> {
        task,
        job: job_id,
        by_ord: ords.into_iter().zip(todo).collect(),
        answer: PhantomData,
    };
    let runner = Runner {
        pool,
        config,
        name,
        job_id,
        resume_after: job.as_ref().and_then(|j| j.resume_after),
    };
    match job::run(&runner, &handler, queue, options).await? {
        Outcome::Finished => Ok(Some(db::llm::cached(pool, &keys).await?)),
        Outcome::Paused | Outcome::DryRun => Ok(None),
    }
}
