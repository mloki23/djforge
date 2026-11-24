//! Background jobs (`job.*`): commands whose work takes seconds or minutes (mixforge's planner,
//! suggestions and analysis, the library health scan) run it on a thread of their own and return
//! `{job}` at once, so the app's UI thread is never held. `job.status` gives a job's progress and,
//! once it ends, its result or error; `job.cancel` asks it to stop. Finished jobs are kept until
//! [`MAX_KEPT`] newer ones have finished.

use super::*;
use crate::cmd;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Finished jobs kept for `job.status`.
const MAX_KEPT: usize = 32;
/// Jobs that may run at once.
const MAX_RUNNING: usize = 4;

/// What a job's work is given: a place for its progress and the cancel flag.
#[derive(Clone)]
pub(crate) struct JobCtx {
    progress: Arc<Mutex<Value>>,
    cancel: Arc<AtomicBool>,
}

impl JobCtx {
    /// Replaces the job's progress (any JSON: `{done, total, current}` by convention).
    pub(crate) fn progress(&self, v: Value) {
        if let Ok(mut p) = self.progress.lock() {
            *p = v;
        }
    }

    /// Whether the job was asked to stop.
    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

enum State {
    Running(JoinHandle<Result<Value>>),
    Done(std::result::Result<Value, String>),
}

struct Job {
    id: u64,
    kind: &'static str,
    started: Instant,
    ended: Option<Instant>,
    ctx: JobCtx,
    state: State,
}

/// The engine's jobs.
#[derive(Default)]
pub struct Jobs {
    next: u64,
    jobs: Vec<Job>,
}

impl Jobs {
    /// Takes the results of jobs that ended, and forgets the oldest finished past [`MAX_KEPT`].
    fn reap(&mut self) {
        for j in &mut self.jobs {
            if matches!(&j.state, State::Running(h) if h.is_finished()) {
                let state = std::mem::replace(&mut j.state, State::Done(Err(String::new())));
                if let State::Running(h) = state {
                    j.state = State::Done(match h.join() {
                        Ok(r) => r.map_err(|e| e.to_string()),
                        Err(_) => Err(format!("the {} job failed", j.kind)),
                    });
                    j.ended = Some(Instant::now());
                }
            }
        }
        let done = self.jobs.iter().filter(|j| matches!(j.state, State::Done(_))).count();
        let mut extra = done.saturating_sub(MAX_KEPT);
        self.jobs.retain(|j| {
            if extra > 0 && matches!(j.state, State::Done(_)) {
                extra -= 1;
                false
            } else {
                true
            }
        });
    }

    fn running(&self) -> usize {
        self.jobs.iter().filter(|j| matches!(j.state, State::Running(_))).count()
    }

    fn report(j: &Job) -> Value {
        let progress = j.ctx.progress.lock().map(|p| p.clone()).unwrap_or(Value::Null);
        let seconds = j.ended.unwrap_or_else(Instant::now).duration_since(j.started).as_secs_f64();
        let mut v = json!({"job": j.id, "kind": j.kind, "progress": progress, "seconds": (seconds * 10.0).round() / 10.0});
        let (state, extra) = match &j.state {
            State::Running(_) => (if j.ctx.cancelled() { "stopping" } else { "running" }, None),
            State::Done(Ok(r)) => ("done", Some(("result", r.clone()))),
            State::Done(Err(e)) if j.ctx.cancelled() => ("cancelled", Some(("error", json!(e)))),
            State::Done(Err(e)) => ("failed", Some(("error", json!(e)))),
        };
        if let Some(o) = v.as_object_mut() {
            o.insert("state".into(), json!(state));
            if let Some((k, x)) = extra {
                o.insert(k.into(), x);
            }
        }
        v
    }
}

/// Starts `work` as a `kind` job for command `cmd`: returns `{job, kind, state: running}` at once.
pub(crate) fn start(e: &mut Engine, cmd: &str, kind: &'static str, work: impl FnOnce(&JobCtx) -> Result<Value> + Send + 'static) -> Result<Value> {
    let jobs = &mut e.jobs;
    jobs.reap();
    if jobs.running() >= MAX_RUNNING {
        return Err(bad(cmd, format!("{MAX_RUNNING} jobs are running: wait for one to end (job.list) or cancel one (job.cancel)")));
    }
    jobs.next += 1;
    let id = jobs.next;
    let ctx = JobCtx { progress: Arc::new(Mutex::new(Value::Null)), cancel: Arc::new(AtomicBool::new(false)) };
    let theirs = ctx.clone();
    let handle = std::thread::Builder::new()
        .name(format!("job-{kind}"))
        .spawn(move || work(&theirs))
        .map_err(|err| EngineError::Io(format!("could not start the {kind} job: {err}")))?;
    jobs.jobs.push(Job { id, kind, started: Instant::now(), ended: None, ctx, state: State::Running(handle) });
    Ok(json!({"job": id, "kind": kind, "state": "running"}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "job.status",
            "Job Status",
            [],
            None,
            "{job} — a background job's state: {job, kind, state: running|stopping|done|failed|cancelled, progress (the job's own, e.g. {done, total, current}), seconds, result (done) or error (failed, cancelled)}. Commands that take a while (library.suggest, library.analyze, library.health, dj.plan_set) return {job} at once and run off the UI thread",
            always,
            status
        ),
        cmd!(
            query "job.list",
            "Job List",
            [],
            None,
            "{} — the jobs running and the last finished ones, each as job.status gives it (without results)",
            always,
            list
        ),
        cmd!(noundo "job.cancel", "Cancel Job", [], None, "{job} — asks a running job to stop (it ends as cancelled at its next check)", always, cancel),
    ]
}

fn job_param(cmd: &str, p: &Value) -> Result<u64> {
    p.get("job").and_then(Value::as_u64).ok_or_else(|| bad(cmd, "`job` required: the id a command returned"))
}

fn find<'a>(e: &'a mut Engine, cmd: &str, p: &Value) -> Result<&'a Job> {
    let id = job_param(cmd, p)?;
    e.jobs.reap();
    e.jobs.jobs.iter().find(|j| j.id == id).ok_or_else(|| bad(cmd, format!("no job {id} (finished jobs are kept for a while; see job.list)")))
}

fn status(e: &mut Engine, p: &Value) -> Result<Value> {
    find(e, "job.status", p).map(Jobs::report)
}

fn list(e: &mut Engine, _: &Value) -> Result<Value> {
    e.jobs.reap();
    let jobs: Vec<Value> = e
        .jobs
        .jobs
        .iter()
        .map(|j| {
            let mut r = Jobs::report(j);
            if let Some(o) = r.as_object_mut() {
                o.remove("result");
            }
            r
        })
        .collect();
    Ok(json!({"jobs": jobs}))
}

fn cancel(e: &mut Engine, p: &Value) -> Result<Value> {
    let j = find(e, "job.cancel", p)?;
    let running = matches!(j.state, State::Running(_));
    if running {
        j.ctx.cancel.store(true, Ordering::Relaxed);
    }
    Ok(json!({"job": j.id, "cancelling": running}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn wait(e: &mut Engine, id: u64) -> Value {
        let start = Instant::now();
        loop {
            let s = e.execute("job.status", &json!({"job": id})).unwrap();
            if s["state"] != "running" && s["state"] != "stopping" {
                return s;
            }
            assert!(start.elapsed().as_secs() < 10, "{s}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_job_reports_progress_then_its_result_and_can_be_cancelled() {
        let mut e = Engine::default();
        let v = start(&mut e, "test", "test", |ctx| {
            ctx.progress(json!({"done": 1, "total": 2}));
            Ok(json!({"answer": 42}))
        })
        .unwrap();
        let s = wait(&mut e, v["job"].as_u64().unwrap());
        assert_eq!(s["state"], "done");
        assert_eq!(s["result"]["answer"], 42);
        assert_eq!(s["progress"]["done"], 1);

        let v = start(&mut e, "test", "slow", |ctx| {
            while !ctx.cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(EngineError::Io("stopped".into()))
        })
        .unwrap();
        let id = v["job"].as_u64().unwrap();
        assert_eq!(e.execute("job.cancel", &json!({"job": id})).unwrap()["cancelling"], true);
        assert_eq!(wait(&mut e, id)["state"], "cancelled");
        assert_eq!(e.execute("job.list", &json!({})).unwrap()["jobs"].as_array().unwrap().len(), 2);
        assert!(e.execute("job.status", &json!({"job": 99})).is_err());
    }
}
