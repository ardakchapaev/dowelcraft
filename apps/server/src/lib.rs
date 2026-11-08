//! Headless build service: a project (or nothing) plus design commands in, files out.
//! Used by the HTTP server (`dowel-server serve`) so a web configurator can produce Bazis models
//! on a Linux server without Bazis itself.

use base64::Engine;
use dowel::design::{self, Project, Session};
use dowel::job::{self, Job};
use dowel::nest::Params;
use dowel::{panel, report};
use serde_json::{Value, json};

const DEFAULT_SHEET: (f64, f64) = (2800.0, 2070.0);

/// Commands that read server files (`furniture.add` takes a filesystem path) are not available over HTTP.
const DENIED: &[&str] = &["furniture.add"];
/// Request limits: commands per request, any number in parameters (mm or counts), copies per command, parts per model.
const MAX_COMMANDS: usize = 500;
const MAX_NUMBER: f64 = 100_000.0;
const MAX_COPIES: f64 = 100.0;
const MAX_PARTS: usize = 5_000;

/// Rejects numbers that would make a model absurdly large or slow (also nested in arrays and objects).
fn check_numbers(v: &Value, key: &str) -> Result<(), String> {
    match v {
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(f64::INFINITY);
            if !x.is_finite() || x.abs() > MAX_NUMBER {
                return Err(format!("“{key}” is out of range (|value| ≤ {MAX_NUMBER})"));
            }
            if key == "count" && x > MAX_COPIES {
                return Err(format!("“count” is limited to {MAX_COPIES}"));
            }
            Ok(())
        }
        Value::Array(a) => a.iter().try_for_each(|x| check_numbers(x, key)),
        Value::Object(o) => o.iter().try_for_each(|(k, x)| check_numbers(x, k)),
        _ => Ok(()),
    }
}

/// Every output kind `build` can produce.
pub const OUTPUTS: &[(&str, &str)] = &[
    ("b3d", "Bazis-Mebelshchik model (.b3d) without the Bazis signature tail — check in real Bazis"),
    ("cfrn", "BAZIS-Cloud exchange model (.cfrn, documented format; the Cloud API converts it to .b3d)"),
    ("project", "the dowel-craft project (.dcraft JSON)"),
    ("spec_csv", "parts list (CSV)"),
    ("smeta_csv", "board and edge banding usage (CSV)"),
    ("raskroy_xml", "BAZIS-Raskroy project (XML)"),
    ("cut_pdf", "cutting maps and panel cards (PDF)"),
    ("drawings_pdf", "assembly and part drawings (PDF)"),
];

/// A failed request: HTTP status and a JSON body.
#[derive(Debug)]
pub struct Failure {
    pub status: u16,
    pub body: Value,
}

fn bad(status: u16, msg: impl Into<String>) -> Failure {
    Failure { status, body: json!({ "error": msg.into() }) }
}

/// Request body:
/// `{ "name"?: str, "project"?: dcraft JSON (object or string), "commands": [{"cmd": str, "params"?: {}}],
///    "outputs"?: [kinds] (default ["b3d"]) }`.
/// Commands run in order; the first failing one stops the build (nothing is produced).
pub fn run(req: &Value) -> Result<(Project, Vec<Value>), Failure> {
    let project = match req.get("project") {
        None | Some(Value::Null) => Project::new(req.get("name").and_then(Value::as_str).unwrap_or("model")),
        Some(Value::String(s)) => Project::load(s).map_err(|e| bad(400, format!("project: {e}")))?,
        Some(v) => Project::load(&v.to_string()).map_err(|e| bad(400, format!("project: {e}")))?,
    };
    let mut s = Session::new(project);
    let mut results = Vec::new();
    let cmds = match req.get("commands") {
        None => &Vec::new(),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(bad(400, "commands must be an array")),
    };
    if cmds.len() > MAX_COMMANDS {
        return Err(bad(413, format!("at most {MAX_COMMANDS} commands per request")));
    }
    for (i, c) in cmds.iter().enumerate() {
        let name = c.get("cmd").and_then(Value::as_str).ok_or_else(|| bad(400, format!("commands[{i}]: cmd is missing")))?;
        if DENIED.contains(&name) {
            return Err(Failure { status: 403, body: json!({ "error": "command not available over HTTP", "command_index": i, "cmd": name }) });
        }
        let params = c.get("params").cloned().unwrap_or_else(|| json!({}));
        check_numbers(&params, "params").map_err(|e| Failure { status: 400, body: json!({ "error": e, "command_index": i, "cmd": name }) })?;
        match s.execute(name, &params) {
            Ok(v) => results.push(v),
            Err(e) => return Err(Failure { status: 422, body: json!({ "error": e, "command_index": i, "cmd": name }) }),
        }
    }
    let parts = design::build(&s.project).map_err(|e| bad(422, e))?.len();
    if parts > MAX_PARTS {
        return Err(bad(413, format!("the model has {parts} parts, at most {MAX_PARTS}")));
    }
    Ok((s.project, results))
}

/// One output of a project as bytes.
pub fn render(project: &Project, kind: &str) -> Result<Vec<u8>, String> {
    let job = || Job::from_project(project.clone());
    let sheet = |j: &Job, m: &str| j.sheet(m, DEFAULT_SHEET);
    Ok(match kind {
        "b3d" => dowel::export::b3d(project)?,
        "cfrn" => dowel::cfrn::cfrn(project)?,
        "project" => project.save().into_bytes(),
        "spec_csv" => report::spec_csv(&panel::spec(&job().panels)).into_bytes(),
        "smeta_csv" => {
            let j = job();
            report::materials_csv(&j.estimate_by(|m| j.coef(m, 1.2))).into_bytes()
        }
        "raskroy_xml" => {
            let j = job();
            dowel::raskroy::project_xml(&j.name, &j.panels, |m| sheet(&j, m))
        }
        "cut_pdf" => {
            let j = job();
            let cuts = job::cut(&j, |m| Params { sheet: sheet(&j, m), ..j.params(m, Params { sheet: DEFAULT_SHEET, kerf: 4.0, trim: 10.0 }) });
            dowel::pdf::from_svg_pages(&job::cards(&j, &cuts))?
        }
        "drawings_pdf" => {
            let j = job();
            let parts = job::parts(&j)?;
            dowel::pdf::from_svg_pages(&dowel::drawing::drawings(&parts, &j.name))?
        }
        other => return Err(format!("unknown output “{other}”")),
    })
}

/// The whole build as JSON: command results, model check, part count and base64 files.
pub fn build(req: &Value) -> Result<Value, Failure> {
    let (project, results) = run(req)?;
    let kinds: Vec<String> = match req.get("outputs") {
        None => vec!["b3d".into()],
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(_) => return Err(bad(400, "outputs must be an array")),
    };
    let parts = design::build(&project).map_err(|e| bad(422, e))?.len();
    let problems: Vec<Value> = design::analysis::analyze(&project)
        .map_err(|e| bad(422, e))?
        .into_iter()
        .map(|p| json!({ "kind": p.kind, "message": p.message, "parts": p.parts }))
        .collect();
    let mut files = serde_json::Map::new();
    for k in &kinds {
        let bytes = render(&project, k).map_err(|e| bad(422, format!("{k}: {e}")))?;
        files.insert(k.clone(), Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)));
    }
    Ok(json!({ "name": project.name, "parts": parts, "results": results, "problems": problems, "files": files }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cabinet_request(outputs: Value) -> Value {
        json!({
            "name": "test",
            "commands": [
                {"cmd": "material.add", "params": {"name": "White 16", "thickness": 16}},
                {"cmd": "cabinet.add", "params": {"width": 800, "height": 720, "depth": 560, "material": "White 16"}},
                {"cmd": "shelf.add", "params": {"cabinet": 1, "height": 360}}
            ],
            "outputs": outputs
        })
    }

    #[test]
    fn builds_a_cabinet_and_its_b3d_reads_back() {
        let out = build(&cabinet_request(json!(["b3d"]))).unwrap();
        assert!(out["parts"].as_u64().unwrap() >= 5, "{out}");
        let b = base64::engine::general_purpose::STANDARD.decode(out["files"]["b3d"].as_str().unwrap()).unwrap();
        let file = dowel::b3d::parse(&b).unwrap();
        assert!(file.section("Document").is_some());
        assert_eq!(dowel::panel::panels(&file).panels.len() as u64, out["parts"].as_u64().unwrap());
    }

    #[test]
    fn every_output_kind_renders() {
        let kinds: Vec<&str> = OUTPUTS.iter().map(|(k, _)| *k).collect();
        let out = build(&cabinet_request(json!(kinds))).unwrap();
        for k in kinds {
            assert!(!out["files"][k].as_str().unwrap().is_empty(), "{k} is empty");
        }
    }

    #[test]
    fn file_reading_commands_and_huge_numbers_are_refused() {
        let e = build(&json!({"commands": [{"cmd": "furniture.add", "params": {"path": "/etc/passwd"}}]})).unwrap_err();
        assert_eq!(e.status, 403);
        let e = build(&json!({"commands": [{"cmd": "material.add", "params": {"name": "M", "thickness": 1e9}}]})).unwrap_err();
        assert_eq!(e.status, 400);
        let e = build(&json!({"commands": [{"cmd": "object.copy", "params": {"ids": [1], "d": [1, 0, 0], "count": 5000}}]})).unwrap_err();
        assert_eq!(e.status, 400);
        let many: Vec<Value> = (0..=MAX_COMMANDS).map(|_| json!({"cmd": "undo"})).collect();
        assert_eq!(build(&json!({"commands": many})).unwrap_err().status, 413);
    }

    #[test]
    fn a_failing_command_reports_its_index() {
        let req = json!({"commands": [{"cmd": "cabinet.add", "params": {"width": 800}}]});
        let e = build(&req).unwrap_err();
        assert_eq!(e.status, 422);
        assert_eq!(e.body["command_index"], 0);
    }
}
