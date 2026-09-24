use anyhow::{bail, Result};
use photo_publisher_contract_validator::{compile_schema, load_json, validate_value};
use photo_publisher_core::{load_state, plan_sync, scan_jpegs, SyncAction};
use photo_publisher_pipeline::{build_local_gallery, recover_publication, PipelineOptions};
use serde::Serialize;
use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};

mod runtime;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "Photo Publisher CLI v1\n\nUSAGE:\n  photo-publisher <command> <project.json> [flags]\n\nCOMMANDS:\n  validate  Validate project.json and required source path\n  publish   Recover if needed, then publish the local gallery\n  recover   Run publication recovery only\n  inspect   Show read-only publication information\n  version   Show the executable version\n\nFLAGS:\n  --silent   Minimize normal output\n  --verbose  Print detailed diagnostics to stderr\n  --dry-run  Calculate publish result without changing publication files\n  --json     Emit a stable JSON result on stdout\n  --help     Show this help\n\nOUTPUT PATH:\n  The current v1 contract has no output field; output defaults to\n  <project.json parent>/output.\n\nDISTRIBUTION:\n  Place schemas/project.schema.json next to the executable.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Validate,
    Publish,
    Recover,
    Inspect,
    Version,
    Help,
}

#[derive(Debug, Default)]
struct Flags {
    silent: bool,
    verbose: bool,
    dry_run: bool,
    json: bool,
}

#[derive(Debug)]
struct Request {
    command: Command,
    project: Option<PathBuf>,
    flags: Flags,
}

#[derive(Debug, Clone, Copy)]
enum ErrorKind {
    Usage,
    ProjectInvalid,
    ResourceMissing,
    Validation,
    Publication,
    Recovery,
    Internal,
}

impl ErrorKind {
    fn code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::ProjectInvalid => 3,
            Self::ResourceMissing => 4,
            Self::Validation => 5,
            Self::Publication => 6,
            Self::Recovery => 7,
            Self::Internal => 9,
        }
    }
    fn category(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::ProjectInvalid => "project_invalid",
            Self::ResourceMissing => "resource_missing",
            Self::Validation => "validation",
            Self::Publication => "publication_failed",
            Self::Recovery => "recovery_failed",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug)]
struct CliError {
    kind: ErrorKind,
    message: String,
}

type CliResult<T> = std::result::Result<T, CliError>;

impl CliError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    fn from_any(kind: ErrorKind, error: impl std::fmt::Display) -> Self {
        Self::new(kind, error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct ProjectInfo {
    project_id: Option<String>,
    project_name: Option<String>,
    source: Option<String>,
    output: String,
}

#[derive(Debug, Serialize)]
struct InspectInfo {
    project: ProjectInfo,
    generation: Option<String>,
    publication_state: Option<String>,
    journal_present: bool,
    recovery_pending: bool,
    photo_count: Option<usize>,
}

fn main() {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let json_mode = raw_args.iter().any(|arg| arg == "--json");
    let request = match parse_args(raw_args) {
        Ok(request) => request,
        Err(error) => {
            emit_error(ErrorKind::Usage, &error.to_string(), json_mode);
            std::process::exit(ErrorKind::Usage.code());
        }
    };
    if request.command == Command::Help {
        println!("{HELP}");
        return;
    }
    if request.command == Command::Version {
        if request.flags.json {
            println!(
                "{}",
                json!({"ok": true, "command": "version", "data": {"version": VERSION}})
            );
        } else {
            println!("photo-publisher {VERSION}");
        }
        return;
    }
    let project = match request.project.as_deref() {
        Some(project) => project,
        None => {
            emit_error(
                ErrorKind::Internal,
                "project path was not produced by argument parsing",
                request.flags.json,
            );
            std::process::exit(ErrorKind::Internal.code());
        }
    };
    let result = match request.command {
        Command::Validate => validate_command(project),
        Command::Publish => publish_command(project, &request.flags),
        Command::Recover => recover_command(project),
        Command::Inspect => inspect_command(project),
        Command::Version | Command::Help => unreachable!(),
    };
    match result {
        Ok(data) => emit_success(command_name(request.command), data, &request.flags),
        Err(error) => {
            emit_error(error.kind, &error.message, request.flags.json);
            std::process::exit(error.kind.code());
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Request> {
    if args.is_empty() {
        bail!("command and project.json are required (use --help)");
    }
    let mut flags = Flags::default();
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--silent" => flags.silent = true,
            "--verbose" => flags.verbose = true,
            "--dry-run" => flags.dry_run = true,
            "--json" => flags.json = true,
            "--help" | "-h" => {
                return Ok(Request {
                    command: Command::Help,
                    project: None,
                    flags,
                })
            }
            _ if arg.starts_with('-') => bail!("unknown flag {arg}"),
            _ => positional.push(arg),
        }
    }
    if flags.silent && flags.verbose {
        bail!("--silent and --verbose cannot be combined");
    }
    let command = match positional.first().map(String::as_str) {
        Some("validate") => Command::Validate,
        Some("publish") => Command::Publish,
        Some("recover") => Command::Recover,
        Some("inspect") => Command::Inspect,
        Some("version") => Command::Version,
        Some(other) => bail!("unknown command {other}"),
        None => bail!("command is required"),
    };
    if command == Command::Version {
        if positional.len() > 1 {
            bail!("version does not accept a project path");
        }
        return Ok(Request {
            command,
            project: None,
            flags,
        });
    }
    if positional.len() != 2 {
        bail!("exactly one project.json path is required");
    }
    if flags.dry_run && command != Command::Publish {
        bail!("--dry-run is only valid with publish");
    }
    Ok(Request {
        command,
        project: Some(PathBuf::from(&positional[1])),
        flags,
    })
}

fn validate_command(project_path: &Path) -> CliResult<Value> {
    let project = load_project(project_path)?;
    let info = project_info(project_path, &project);
    let source = required_source(project_path, &project)?;
    if !source.is_dir() {
        return Err(CliError::new(
            ErrorKind::ResourceMissing,
            format!("source directory does not exist: {}", source.display()),
        ));
    }
    Ok(json!({"valid": true, "project": info, "source_exists": true}))
}

fn publish_command(project_path: &Path, flags: &Flags) -> CliResult<Value> {
    let project = load_project(project_path)?;
    let source = required_source(project_path, &project)?;
    if !source.is_dir() {
        return Err(CliError::new(
            ErrorKind::ResourceMissing,
            format!("source directory does not exist: {}", source.display()),
        ));
    }
    let output = output_path(project_path);
    let title = project["gallery"]["title"]
        .as_str()
        .ok_or_else(|| CliError::new(ErrorKind::ProjectInvalid, "gallery.title is required"))?;
    if flags.dry_run {
        let previous = load_state(output.join(".publisher/state.json"))
            .map_err(|e| CliError::from_any(ErrorKind::Validation, e))?;
        let current =
            scan_jpegs(&source).map_err(|e| CliError::from_any(ErrorKind::Validation, e))?;
        let plan = plan_sync(&previous, &current);
        let mut adds = 0;
        let mut updates = 0;
        let mut removes = 0;
        for action in plan {
            match action {
                SyncAction::Add(_) => adds += 1,
                SyncAction::Update(_) => updates += 1,
                SyncAction::Remove { .. } => removes += 1,
            }
        }
        // Integrated dry-run view: computed without touching providers and
        // without writing the ledger (null when not applicable).
        let integration = runtime::dry_run_integration(project_path, &output);
        return Ok(
            json!({"dry_run": true, "would_publish": adds + updates + removes > 0 || !output.join("gallery.json").exists(), "plan": {"add": adds, "update": updates, "remove": removes}, "output": output, "integration": integration}),
        );
    }
    // For schema v2 projects, provider configuration and credentials are
    // validated before the local build so misconfiguration never reaches a
    // remote effect.
    let integrated_config = if project["schemaVersion"].as_u64() == Some(2) {
        Some(runtime::preflight(&project, project_path)?)
    } else {
        None
    };
    build_local_gallery(&PipelineOptions {
        source_dir: source.clone(),
        output_dir: output.clone(),
        project_title: title.to_string(),
    })
    .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;
    if let Some(configuration) = integrated_config {
        let integrated = runtime::publish_integrated(&configuration, &output)?;
        return Ok(json!({
            "published": true,
            "source": source,
            "output": output,
            "integrated": integrated,
        }));
    }
    Ok(json!({"published": true, "source": source, "output": output}))
}

fn recover_command(project_path: &Path) -> CliResult<Value> {
    let project = load_project(project_path)?;
    let output = output_path(project_path);
    let recovered =
        recover_publication(&output).map_err(|e| CliError::from_any(ErrorKind::Recovery, e))?;
    Ok(
        json!({"recovery_attempted": true, "recovery_needed": recovered.as_ref().is_some_and(|r| !matches!(r.phase, photo_publisher_pipeline::journal::JournalPhase::Committed)), "generation": recovered.map(|r| r.generation), "output": output, "project": project_info(project_path, &project)}),
    )
}

fn inspect_command(project_path: &Path) -> CliResult<Value> {
    let project = load_project(project_path)?;
    let output = output_path(project_path);
    let journal_path = output.join(".publisher/journal.json");
    let journal = if journal_path.is_file() {
        Some(
            photo_publisher_pipeline::journal::read_journal(&journal_path).map_err(|e| {
                CliError::from_any(ErrorKind::Recovery, format!("invalid journal: {e}"))
            })?,
        )
    } else {
        None
    };
    let state_path = output.join(".publisher/state.json");
    let state = if state_path.is_file() {
        Some(load_state(&state_path).map_err(|e| {
            CliError::from_any(ErrorKind::Validation, format!("invalid state: {e}"))
        })?)
    } else {
        None
    };
    let recovery_pending = journal.as_ref().is_some_and(|r| {
        !matches!(
            r.phase,
            photo_publisher_pipeline::journal::JournalPhase::Committed
        )
    });
    let info = InspectInfo {
        project: project_info(project_path, &project),
        generation: journal.as_ref().map(|r| r.generation.clone()),
        publication_state: journal.as_ref().map(|r| format!("{:?}", r.phase)),
        journal_present: journal_path.is_file(),
        recovery_pending,
        photo_count: state.map(|s| s.photos.len()),
    };
    serde_json::to_value(info).map_err(|e| CliError::from_any(ErrorKind::Internal, e))
}

fn load_project(path: &Path) -> CliResult<Value> {
    if !path.is_file() {
        return Err(CliError::new(
            ErrorKind::ResourceMissing,
            format!("project file does not exist: {}", path.display()),
        ));
    }
    let project = load_json(path).map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))?;
    let schema = schema_path()?;
    let validator =
        compile_schema(&schema).map_err(|e| CliError::from_any(ErrorKind::Internal, e))?;
    validate_value(&validator, &project).map_err(|e| {
        CliError::from_any(
            ErrorKind::ProjectInvalid,
            format!("project.json contract validation failed: {e}"),
        )
    })?;
    Ok(project)
}

fn required_source(project_path: &Path, project: &Value) -> CliResult<PathBuf> {
    if project["source"]["type"].as_str() != Some("folder") {
        return Err(CliError::new(
            ErrorKind::ProjectInvalid,
            "project.source.type must be folder for local CLI operations",
        ));
    }
    let raw = project["source"]["path"].as_str().ok_or_else(|| {
        CliError::new(ErrorKind::ProjectInvalid, "project.source.path is required")
    })?;
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        project_path.parent().unwrap_or(Path::new(".")).join(path)
    })
}

fn output_path(project_path: &Path) -> PathBuf {
    project_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("output")
}

fn project_info(project_path: &Path, project: &Value) -> ProjectInfo {
    ProjectInfo {
        project_id: project["project"]["id"].as_str().map(str::to_owned),
        project_name: project["project"]["name"].as_str().map(str::to_owned),
        source: project["source"]["path"].as_str().map(str::to_owned),
        output: output_path(project_path).display().to_string(),
    }
}

fn schema_path() -> CliResult<PathBuf> {
    let executable = env::current_exe().map_err(|e| CliError::from_any(ErrorKind::Internal, e))?;
    let mut directory = executable
        .parent()
        .ok_or_else(|| CliError::new(ErrorKind::Internal, "executable has no parent directory"))?
        .to_path_buf();
    loop {
        let candidate = directory.join("schemas/project.schema.json");
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !directory.pop() {
            break;
        }
    }
    Err(CliError::new(
        ErrorKind::ResourceMissing,
        format!(
            "schema resource not found beside executable: {}",
            executable.display()
        ),
    ))
}

fn command_name(command: Command) -> &'static str {
    match command {
        Command::Validate => "validate",
        Command::Publish => "publish",
        Command::Recover => "recover",
        Command::Inspect => "inspect",
        Command::Version => "version",
        Command::Help => "help",
    }
}

fn emit_success(command: &str, data: Value, flags: &Flags) {
    if flags.json {
        println!("{}", json!({"ok": true, "command": command, "data": data}));
        return;
    }
    if !flags.silent {
        println!("{command}: ok");
        if flags.verbose {
            eprintln!("result: {}", data);
        }
    }
}

fn emit_error(kind: ErrorKind, message: &str, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            json!({"ok": false, "error": {"code": kind.code(), "category": kind.category(), "message": message}})
        );
    } else {
        eprintln!("photo-publisher: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_flags_and_publish() {
        let request = parse_args(vec![
            "publish".into(),
            "project.json".into(),
            "--json".into(),
        ])
        .unwrap();
        assert_eq!(request.command, Command::Publish);
        assert!(request.flags.json);
    }
    #[test]
    fn rejects_unknown_command() {
        assert!(parse_args(vec!["wat".into()]).is_err());
    }
    #[test]
    fn version_does_not_require_project() {
        assert_eq!(
            parse_args(vec!["version".into()]).unwrap().command,
            Command::Version
        );
    }
}
