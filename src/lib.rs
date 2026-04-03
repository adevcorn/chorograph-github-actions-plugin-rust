// chorograph-github-actions-plugin-rust
//
// WASM plugin that polls GitHub Actions workflow runs via the `gh` CLI and
// emits normalized CI status events to the Chorograph Swift host via
// `host_push_ai_event`.
//
// Normalized event protocol (sent as raw JSON, NOT wrapped in AIEvent):
//
//   // Polled every ~30 s via detect_run_status:
//   {"type":"ciWorkflowRuns",
//    "workflowPath":".github/workflows/ci.yml",
//    "workflowName":"CI",
//    "runs":[
//      {"runId":12345,
//       "status":"completed",
//       "conclusion":"success",
//       "branch":"main",
//       "event":"push",
//       "updatedAt":"2026-04-03T12:00:00Z"}
//    ]}
//
//   // On-demand via handle_action("fetch_ci_jobs", {runId: <N>}):
//   {"type":"ciJobDetails",
//    "runId":12345,
//    "jobs":[
//      {"name":"build","status":"completed","conclusion":"success","durationSeconds":12},
//      {"name":"test","status":"completed","conclusion":"failure","durationSeconds":8}
//    ]}

use chorograph_plugin_sdk_rust::prelude::*;
use serde_json::json;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Plugin lifecycle
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    let ui = json!([
        { "type": "label", "text": "GitHub Actions" }
    ]);
    push_ui(&ui.to_string());
}

// ---------------------------------------------------------------------------
// Run-status polling hook (called every ~30 s by RunStatusPoller)
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn detect_run_status(workspace_root: String) -> RunStatus {
    // List all workflow files in .github/workflows/
    let workflows_dir = format!("{}/.github/workflows", workspace_root.trim_end_matches('/'));
    let wf_list = list_workflow_files(&workflows_dir);

    if wf_list.is_empty() {
        // Not a GitHub-backed workspace or no workflows defined — stay quiet.
        return RunStatus {
            is_running: false,
            url: None,
            pid: None,
            resources: vec![],
        };
    }

    for wf_path in &wf_list {
        // Derive the repo-relative path for the API and the display name.
        // wf_path is absolute; strip workspace_root prefix to get the relative part.
        let rel = wf_path
            .strip_prefix(&workspace_root)
            .unwrap_or(wf_path.as_str())
            .trim_start_matches('/');

        let workflow_name = workflow_display_name(wf_path);

        // gh run list --workflow <rel-path> --json databaseId,status,conclusion,headBranch,event,updatedAt --limit 5
        let runs = fetch_runs(wf_path, rel, &workflow_name, &workspace_root);
        if !runs.is_empty() {
            push_raw_event("", &runs);
        }
    }

    RunStatus {
        is_running: false,
        url: None,
        pid: None,
        resources: vec![],
    }
}

// ---------------------------------------------------------------------------
// Action handler — on-demand job details
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn handle_action(action_id: String, payload: serde_json::Value) {
    if action_id != "fetch_ci_jobs" {
        return;
    }

    let run_id = match payload.get("runId").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => {
            log!("[github-actions] fetch_ci_jobs: missing runId in payload");
            return;
        }
    };

    let workspace_root = payload
        .get("workspaceRoot")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    fetch_jobs(run_id, &workspace_root);
}

// ---------------------------------------------------------------------------
// Workflow file discovery
// ---------------------------------------------------------------------------

fn list_workflow_files(workflows_dir: &str) -> Vec<String> {
    // Use `ls` to enumerate *.yml / *.yaml in the workflows directory.
    // This avoids any host-FS API calls beyond simple child process I/O.
    let child = match ChildProcess::spawn("ls", vec!["-1", workflows_dir], None, HashMap::new()) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut buf: Vec<u8> = Vec::new();
    loop {
        child.wait_for_data(200);
        match child.read(PipeType::Stdout) {
            Ok(ReadResult::Data(d)) => buf.extend(d),
            _ => break,
        }
        match child.get_status() {
            ProcessStatus::Running => {}
            _ => {
                // One more drain
                loop {
                    match child.read(PipeType::Stdout) {
                        Ok(ReadResult::Data(d)) => buf.extend(d),
                        _ => break,
                    }
                }
                break;
            }
        }
    }

    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .filter(|l| l.ends_with(".yml") || l.ends_with(".yaml"))
        .map(|l| format!("{}/{}", workflows_dir, l.trim()))
        .collect()
}

// ---------------------------------------------------------------------------
// Run fetching
// ---------------------------------------------------------------------------

fn fetch_runs(
    _abs_wf_path: &str,
    rel_wf_path: &str,
    workflow_name: &str,
    workspace_root: &str,
) -> String {
    log!(
        "[github-actions] fetch_runs: rel={} name={}",
        rel_wf_path,
        workflow_name
    );

    let child = match ChildProcess::spawn(
        "gh",
        vec![
            "run",
            "list",
            "--workflow",
            rel_wf_path,
            "--json",
            "databaseId,status,conclusion,headBranch,event,updatedAt",
            "--limit",
            "5",
        ],
        Some(workspace_root),
        HashMap::new(),
    ) {
        Ok(c) => c,
        Err(e) => {
            log!(
                "[github-actions] gh spawn failed for {}: {:?}",
                rel_wf_path,
                e
            );
            return String::new();
        }
    };

    let mut stdout_buf: Vec<u8> = Vec::new();
    loop {
        child.wait_for_data(300);
        loop {
            match child.read(PipeType::Stdout) {
                Ok(ReadResult::Data(d)) => stdout_buf.extend(d),
                _ => break,
            }
        }
        match child.get_status() {
            ProcessStatus::Running => {}
            _ => {
                loop {
                    match child.read(PipeType::Stdout) {
                        Ok(ReadResult::Data(d)) => stdout_buf.extend(d),
                        _ => break,
                    }
                }
                break;
            }
        }
    }

    let raw = String::from_utf8_lossy(&stdout_buf);
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return String::new();
    }

    // Parse the gh JSON array
    let gh_runs: Vec<serde_json::Value> = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            log!(
                "[github-actions] JSON parse error for {}: {}",
                rel_wf_path,
                e
            );
            return String::new();
        }
    };

    let runs: Vec<serde_json::Value> = gh_runs
        .iter()
        .map(|r| {
            json!({
                "runId":      r.get("databaseId").and_then(|v| v.as_i64()).unwrap_or(0),
                "status":     r.get("status").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "conclusion": r.get("conclusion").and_then(|v| v.as_str()).unwrap_or(""),
                "branch":     r.get("headBranch").and_then(|v| v.as_str()).unwrap_or(""),
                "event":      r.get("event").and_then(|v| v.as_str()).unwrap_or(""),
                "updatedAt":  r.get("updatedAt").and_then(|v| v.as_str()).unwrap_or(""),
            })
        })
        .collect();

    let runs_len = runs.len();
    let event = json!({
        "type":         "ciWorkflowRuns",
        "workflowPath": rel_wf_path,
        "workflowName": workflow_name,
        "runs":         runs,
    });

    log!(
        "[github-actions] emitting ciWorkflowRuns for {} ({} runs)",
        rel_wf_path,
        runs_len
    );

    event.to_string()
}

// ---------------------------------------------------------------------------
// Job fetching
// ---------------------------------------------------------------------------

fn fetch_jobs(run_id: i64, workspace_root: &str) {
    log!("[github-actions] fetch_jobs: runId={}", run_id);

    let run_id_str = run_id.to_string();
    let cwd = if workspace_root.is_empty() {
        None
    } else {
        Some(workspace_root)
    };

    let child = match ChildProcess::spawn(
        "gh",
        vec!["run", "view", &run_id_str, "--json", "jobs"],
        cwd,
        HashMap::new(),
    ) {
        Ok(c) => c,
        Err(e) => {
            log!("[github-actions] gh run view spawn failed: {:?}", e);
            return;
        }
    };

    let mut stdout_buf: Vec<u8> = Vec::new();
    loop {
        child.wait_for_data(300);
        loop {
            match child.read(PipeType::Stdout) {
                Ok(ReadResult::Data(d)) => stdout_buf.extend(d),
                _ => break,
            }
        }
        match child.get_status() {
            ProcessStatus::Running => {}
            _ => {
                loop {
                    match child.read(PipeType::Stdout) {
                        Ok(ReadResult::Data(d)) => stdout_buf.extend(d),
                        _ => break,
                    }
                }
                break;
            }
        }
    }

    let raw = String::from_utf8_lossy(&stdout_buf);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    let gh_resp: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            log!("[github-actions] JSON parse error for run view: {}", e);
            return;
        }
    };

    let empty_arr = serde_json::Value::Array(vec![]);
    let gh_jobs = gh_resp
        .get("jobs")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| match &empty_arr {
            serde_json::Value::Array(a) => a,
            _ => unreachable!(),
        });

    let jobs: Vec<serde_json::Value> = gh_jobs
        .iter()
        .map(|j| {
            // Compute duration from startedAt / completedAt if available.
            let duration = compute_duration(
                j.get("startedAt").and_then(|v| v.as_str()).unwrap_or(""),
                j.get("completedAt").and_then(|v| v.as_str()).unwrap_or(""),
            );
            json!({
                "name":            j.get("name").and_then(|v| v.as_str()).unwrap_or("job"),
                "status":          j.get("status").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "conclusion":      j.get("conclusion").and_then(|v| v.as_str()).unwrap_or(""),
                "durationSeconds": duration,
            })
        })
        .collect();

    let jobs_len = jobs.len();
    let event = json!({
        "type":  "ciJobDetails",
        "runId": run_id,
        "jobs":  jobs,
    });

    log!(
        "[github-actions] emitting ciJobDetails for runId={} ({} jobs)",
        run_id,
        jobs_len
    );

    push_raw_event("", &event.to_string());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a human-readable workflow name from its absolute path.
/// ".github/workflows/ci.yml" → "CI"
fn workflow_display_name(abs_path: &str) -> String {
    let file = abs_path.rsplit('/').next().unwrap_or(abs_path);
    // Strip .yml / .yaml extension and title-case
    let stem = file.trim_end_matches(".yaml").trim_end_matches(".yml");
    // Replace hyphens/underscores with spaces and title-case each word
    stem.split(|c| c == '-' || c == '_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Naively parse ISO-8601 timestamps and return duration in seconds.
/// Returns 0 if parsing fails or either timestamp is empty.
fn compute_duration(started_at: &str, completed_at: &str) -> i64 {
    fn parse_epoch(ts: &str) -> Option<i64> {
        // Expect format: 2026-04-03T12:00:00Z
        // Parse manually — no chrono available in WASM without WASI.
        let ts = ts.trim_end_matches('Z');
        let parts: Vec<&str> = ts.splitn(2, 'T').collect();
        if parts.len() != 2 {
            return None;
        }
        let date_parts: Vec<i64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
        let time_parts: Vec<i64> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
        if date_parts.len() < 3 || time_parts.len() < 3 {
            return None;
        }
        let (y, m, d) = (date_parts[0], date_parts[1], date_parts[2]);
        let (hh, mm, ss) = (time_parts[0], time_parts[1], time_parts[2]);
        // Days since Unix epoch (rough Gregorian approximation)
        let days = days_since_epoch(y, m, d);
        Some(days * 86400 + hh * 3600 + mm * 60 + ss)
    }

    match (parse_epoch(started_at), parse_epoch(completed_at)) {
        (Some(s), Some(e)) if e >= s => e - s,
        _ => 0,
    }
}

fn days_since_epoch(y: i64, m: i64, d: i64) -> i64 {
    // Zeller / algorithm from https://en.wikipedia.org/wiki/Julian_day
    // Adjusted to days since 1970-01-01
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let a = y / 100;
    let b = 2 - a + a / 4;
    let jd =
        ((365.25 * (y + 4716) as f64) as i64) + ((30.6001 * (m + 1) as f64) as i64) + d + b - 1524;
    // Julian day of 1970-01-01 = 2440588
    jd - 2440588
}

// ---------------------------------------------------------------------------
// Raw-event helper (bypasses the typed AIEvent enum)
// ---------------------------------------------------------------------------

fn push_raw_event(session_id: &str, json: &str) {
    log!(
        "[github-actions] push_raw_event session={} json={}",
        session_id,
        &json[..json.len().min(200)]
    );
    unsafe {
        chorograph_plugin_sdk_rust::ffi::host_push_ai_event(
            session_id.as_ptr(),
            session_id.len() as i32,
            json.as_ptr(),
            json.len() as i32,
        );
    }
}
