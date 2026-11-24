//! The DJ tools through MCP, as an agent uses them: search the library (a fixture mixforge
//! library in a temporary folder, never the real one), suggest and plan with a fake mixforge,
//! health, jobs that outlast the wait, and the tool list naming them all.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use soundcraft_automation::{Headless, mcp::Server};
use soundcraft_engine::Engine;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-mcp-dj-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rpc(s: &mut Server<Headless>, method: &str, params: Value) -> Value {
    s.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).unwrap()
}

/// A tool's answer: (is_error, its JSON or text).
fn call(s: &mut Server<Headless>, name: &str, args: Value) -> (bool, Value) {
    let r = rpc(s, "tools/call", json!({"name": name, "arguments": args}));
    let res = &r["result"];
    let text = res["content"][0]["text"].as_str().unwrap_or_default();
    (res["isError"] == true, serde_json::from_str(text).unwrap_or(Value::String(text.to_string())))
}

fn tool(s: &mut Server<Headless>, name: &str, args: Value) -> Value {
    let (err, v) = call(s, name, args.clone());
    assert!(!err, "{name} {args}: {v}");
    v
}

/// mixforge's schema with a row per (title, bpm, camelot); the files are stand-ins (listing never decodes).
fn library(dir: &Path, tracks: &[(&str, f64, &str)]) -> (PathBuf, Vec<String>) {
    let db = dir.join("library.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT);
         CREATE TABLE structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT);",
    )
    .unwrap();
    let mut paths = Vec::new();
    for (i, (title, bpm, key)) in tracks.iter().enumerate() {
        let path = dir.join(format!("t{i}.wav"));
        std::fs::write(&path, b"stand-in").unwrap();
        let m = std::fs::metadata(&path).unwrap();
        let mtime = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let path = path.to_string_lossy().into_owned();
        let data = json!({"path": path, "title": title, "artist": "Fixture", "bpm": bpm, "first_downbeat": 0.0, "duration": 300.0,
            "audio_end": 300.0, "camelot": key, "key": "A minor", "bars": 160, "constant_tempo": true, "energy": 6.0});
        c.execute("INSERT INTO tracks VALUES (?1, ?2, ?3, 9, ?4)", rusqlite::params![path, m.len() as i64, mtime, data.to_string()]).unwrap();
        paths.push(path);
    }
    (db, paths)
}

#[test]
fn an_agent_searches_suggests_plans_and_checks_the_library_through_mcp() {
    let dir = Scratch::new("flow");
    let (db, paths) = library(&dir.0, &[("Rise", 138.0, "8A"), ("Glow", 140.0, "9A"), ("Fall", 128.0, "3B")]);
    let mut e = Engine::default();
    e.set_library(db);
    // mixforge, faked: suggestions name the next track; a plan writes its file; analysis is slow.
    let next = paths[1].clone();
    e.set_agent(Arc::new(move |op: &str, args: &Value, progress: &dyn Fn(Value), stop: &dyn Fn() -> bool| match op {
        "suggest" => Ok(json!([{"path": next, "title": "Glow", "relation": "+1 up", "score": 1.2}])),
        "plan" => {
            let out = args["out"].as_str().unwrap();
            std::fs::write(out, b"{}").unwrap();
            Ok(json!({"plan": out, "tracks": args["sources"], "warnings": []}))
        }
        _ => {
            while !stop() {
                progress(json!({"stage": "analysing", "done": 0, "total": 1}));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err("cancelled".into())
        }
    }));
    let mut s = Server::new(Headless::new(e));
    let tools: Vec<String> =
        rpc(&mut s, "tools/list", json!({}))["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for t in [
        "library_search",
        "suggest_tracks",
        "plan_set",
        "import_set",
        "set_overview",
        "prep_tracks",
        "export_set",
        "library_health",
        "analyze_tracks",
        "job_status",
        "cancel_job",
    ] {
        assert!(tools.iter().any(|n| n == t), "no {t} tool");
    }
    // Search: waits out the library's first read; filters by tempo and key.
    let v = tool(&mut s, "library_search", json!({"bpm_min": 135, "keys": ["8A", "9A"]}));
    let titles: Vec<&str> = v["rows"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Glow", "Rise"]);
    // Suggest and plan: jobs, waited for.
    let v = tool(&mut s, "suggest_tracks", json!({"from": paths[0], "direction": "up"}));
    assert_eq!(v[0]["title"], "Glow");
    let out = dir.0.join("set.plan.json").to_string_lossy().into_owned();
    let v = tool(&mut s, "plan_set", json!({"tracks": [paths[0], paths[1]], "out": out, "order": "harmonic"}));
    assert_eq!(v["plan"], json!(out));
    assert!(Path::new(&out).exists());
    // Health: read only, waited for.
    let v = tool(&mut s, "library_health", json!({"folders": false}));
    assert_eq!(v["counts"]["tracks"], 3);
    // A job that outlasts the wait comes back running; job_status waits again; cancel_job stops it.
    let v = tool(&mut s, "analyze_tracks", json!({"paths": [paths[2]], "wait_seconds": 0.2}));
    let job = v["job"].as_u64().unwrap_or_else(|| panic!("{v}"));
    assert_eq!(v["state"], "running");
    assert_eq!(tool(&mut s, "job_status", json!({"job": job, "wait_seconds": 0.1}))["progress"]["stage"], "analysing");
    tool(&mut s, "cancel_job", json!({"job": job}));
    let (err, v) = call(&mut s, "job_status", json!({"job": job, "wait_seconds": 5}));
    assert!(err && v.as_str().unwrap().contains("cancelled"), "{v}");
    // A refused command is an error the agent can read.
    let (err, v) = call(&mut s, "suggest_tracks", json!({"from": paths[0], "direction": "sideways"}));
    assert!(err && v.as_str().unwrap().contains("direction"), "{v}");
}
