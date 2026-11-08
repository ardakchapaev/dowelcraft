//! `dowel-server`: the dowel-craft command line without the window (for Linux servers and Docker)
//! plus `dowel-server serve` — an HTTP API for web configurators.
//!
//! API (JSON in, JSON or a file out):
//!   GET  /health                 → {"ok": true, "version"}
//!   GET  /v1/commands            → design commands with their parameters
//!   GET  /v1/outputs             → output kinds
//!   POST /v1/build               → {name, parts, results, problems, files: {kind: base64}}
//!   POST /v1/b3d                 → the .b3d file itself (application/octet-stream)
//! The body of both POSTs: {name?, project?, commands: [{cmd, params}], outputs?: [kinds]}.
//! When DOWEL_TOKEN is set, requests need `Authorization: Bearer <token>`.

use std::io::Read;
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

const MAX_BODY: usize = 4 * 1024 * 1024;

fn main() -> ExitCode {
    dowel::i18n::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some(cmd) if dowel_craft_cli::is_command(cmd) => dowel_craft_cli::main(&args),
        Some("--version") => {
            println!("dowel-server {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: dowel-server serve [--addr 0.0.0.0:8090] [--threads 4]\n       dowel-server <dowel-craft command> ... (see: dowel-server help)");
            ExitCode::FAILURE
        }
    }
}

fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn serve(args: &[String]) -> ExitCode {
    let addr = opt(args, "--addr").unwrap_or("127.0.0.1:8090");
    let threads: usize = opt(args, "--threads").and_then(|t| t.parse().ok()).unwrap_or(4).max(1);
    let token = std::env::var("DOWEL_TOKEN").ok().filter(|t| !t.is_empty());
    let server = match Server::http(addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("dowel-server: {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("dowel-server listening on http://{addr} ({threads} workers, auth {})", if token.is_some() { "on" } else { "off" });
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (server, token) = (server.clone(), token.clone());
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(req, token.as_deref());
                }
            })
        })
        .collect();
    workers.into_iter().for_each(|w| {
        let _ = w.join();
    });
    ExitCode::SUCCESS
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body.to_string().into_bytes())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap())
}

fn authorized(req: &Request, token: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    let want = format!("Bearer {token}");
    req.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == want)
}

fn read_json(req: &mut Request) -> Result<Value, (u16, String)> {
    let mut body = Vec::new();
    req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).map_err(|e| (400, e.to_string()))?;
    if body.len() > MAX_BODY {
        return Err((413, format!("body over {MAX_BODY} bytes")));
    }
    serde_json::from_slice(&body).map_err(|e| (400, format!("JSON: {e}")))
}

fn handle(mut req: Request, token: Option<&str>) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    let started = std::time::Instant::now();
    let status = if path != "/health" && !authorized(&req, token) {
        respond(req, json_response(401, &json!({"error": "unauthorized"})))
    } else {
        match route(&mut req, &method, &path) {
            Ok(file) => respond_file(req, file),
            Err(r) => respond(req, r),
        }
    };
    eprintln!("{method} {path} {status} {:?}", started.elapsed());
}

/// Ok — a .b3d to send as a file; Err — a JSON response (also for successes).
fn route(req: &mut Request, method: &Method, path: &str) -> Result<(String, Vec<u8>), Response<std::io::Cursor<Vec<u8>>>> {
    let list = |items: &[(&str, &str)]| json!(items.iter().map(|(n, h)| json!({"name": n, "help": h})).collect::<Vec<_>>());
    match (method, path) {
        (Method::Get, "/health") => Err(json_response(200, &json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))),
        (Method::Get, "/v1/commands") => Err(json_response(200, &list(dowel::design::COMMANDS))),
        (Method::Get, "/v1/outputs") => Err(json_response(200, &list(dowel_server::OUTPUTS))),
        (Method::Post, "/v1/build" | "/v1/b3d") => {
            let body = read_json(req).map_err(|(s, e)| json_response(s, &json!({"error": e})))?;
            if path == "/v1/build" {
                return Err(match dowel_server::build(&body) {
                    Ok(v) => json_response(200, &v),
                    Err(f) => json_response(f.status, &f.body),
                });
            }
            let (project, _) = dowel_server::run(&body).map_err(|f| json_response(f.status, &f.body))?;
            let bytes = dowel_server::render(&project, "b3d").map_err(|e| json_response(422, &json!({"error": e})))?;
            Ok((format!("{}.b3d", project.name), bytes))
        }
        _ => Err(json_response(404, &json!({"error": "not found"}))),
    }
}

fn respond(req: Request, r: Response<std::io::Cursor<Vec<u8>>>) -> u16 {
    let code = r.status_code().0;
    let _ = req.respond(r);
    code
}

fn respond_file(req: Request, (name, bytes): (String, Vec<u8>)) -> u16 {
    // ASCII fallback name plus the UTF-8 one (RFC 5987) for Cyrillic project names.
    let ascii: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    let encoded: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b == b'.' { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    let disposition = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    let r = Response::from_data(bytes)
        .with_header(Header::from_bytes("Content-Type", "application/octet-stream").unwrap())
        .with_header(Header::from_bytes("Content-Disposition", disposition).unwrap());
    respond(req, r)
}
