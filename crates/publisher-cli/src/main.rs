use anyhow::{bail, Result};
use photo_publisher_core::{load_state, plan_sync, scan_jpegs, SyncAction};
use photo_publisher_pipeline::{build_local_gallery, PipelineOptions};
use serde::Serialize;
use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};

mod runtime;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "Photo Publisher CLI v1\n\nUSAGE:\n  photo-publisher <command> <project.json> [flags]\n\nCOMMANDS:\n  validate  Validate project.json and required source path\n  publish   Recover if needed, then publish the local gallery\n  recover   Run publication recovery only\n  inspect   Show read-only publication information\n  version   Show the executable version\n\nFLAGS:\n  --silent   Minimize normal output\n  --verbose  Print detailed diagnostics to stderr\n  --dry-run  Calculate publish result without changing publication files\n  --json     Emit a stable JSON result on stdout\n  --help     Show this help\n\nOUTPUT PATH:\n  The current v1 contract has no output field; output defaults to\n  <project.json parent>/output.\n\nDISTRIBUTION:\n  Schemas are embedded in the executable; no schema directory is\n  required beside it.\n";

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

/// Errors produced by the application layer carry their own stable category;
/// converting to the CLI error preserves it exactly (including exit codes).
impl From<publisher_app::ApplicationError> for CliError {
    fn from(error: publisher_app::ApplicationError) -> Self {
        map_app_error(error)
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
    let outcome = publisher_app::validate_project(project_path, &mut |_event| {})?;
    Ok(json!({
        "valid": true,
        "project": ProjectInfo {
            project_id: Some(outcome.project_id),
            project_name: Some(outcome.project_name),
            source: Some(outcome.source_declared),
            output: outcome.output_dir.display().to_string(),
        },
        "source_exists": true,
    }))
}

/// Maps application outcomes to the long-standing JSON shape of `inspect`.
/// Field names and presence are unchanged.
fn inspect_outcome_to_info(outcome: &publisher_app::InspectOutcome) -> InspectInfo {
    InspectInfo {
        project: ProjectInfo {
            project_id: outcome.project_id.clone(),
            project_name: outcome.project_name.clone(),
            source: outcome.source_declared.clone(),
            output: outcome.output_dir.display().to_string(),
        },
        generation: outcome
            .journal
            .as_ref()
            .map(|journal| journal.generation.clone()),
        publication_state: outcome
            .journal
            .as_ref()
            .map(|journal| format!("{:?}", journal.phase)),
        journal_present: outcome.journal_present,
        recovery_pending: outcome.recovery_pending(),
        photo_count: outcome.photo_count,
    }
}

fn inspect_command(project_path: &Path) -> CliResult<Value> {
    let outcome = publisher_app::inspect_project(project_path, &mut |_event| {})?;
    serde_json::to_value(inspect_outcome_to_info(&outcome))
        .map_err(|e| CliError::from_any(ErrorKind::Internal, e))
}

fn recover_command(project_path: &Path) -> CliResult<Value> {
    let outcome = publisher_app::recover_publication(project_path, &mut |_event| {})?;
    Ok(json!({
        "recovery_attempted": outcome.attempted,
        "recovery_needed": outcome.state.as_ref().is_some_and(|state| state.recovery_needed),
        "generation": outcome.state.as_ref().map(|state| state.generation.clone()),
        "output": outcome.output_dir,
        "project": ProjectInfo {
            project_id: outcome.project_id,
            project_name: outcome.project_name,
            source: outcome.source_declared,
            output: outcome.output_dir.display().to_string(),
        },
    }))
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
        // Integrated dry-run view: computed by the application layer without
        // touching providers and without writing the ledger (null when not
        // applicable, e.g., schema v1 or nothing committed yet).
        let integration = if project["schemaVersion"].as_u64() == Some(2) {
            publisher_app::dry_run_project(project_path, &mut |_| {})
                .map(|outcome| {
                    json!({
                        "planned": true,
                        "operations": {
                            "storage": outcome.storage_operations,
                            "repository": outcome.repository_operations,
                            "hosting": outcome.hosting_operations,
                        },
                        "reconciliation_requirements": outcome.reconciliation_requirements,
                    })
                })
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
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
    if let Some(configuration) = integrated_config {
        // Composition root: concrete providers are built here, and the
        // orchestration runs in the application layer over trait objects.
        let mut wire = runtime::compose_providers(&configuration)?;
        let credentials = runtime::credential_store();
        let outcome = publisher_app::publish_project(
            project_path,
            Some(&configuration),
            publisher_app::PublishOptions,
            &mut publisher_app::PublicationProviders {
                storage: &mut wire.storage,
                repository: &mut wire.repository,
                hosting: &mut wire.hosting,
                credentials: &credentials,
            },
            &mut |_| {},
        )
        .map_err(map_app_error)?;
        return Ok(json!({
            "published": true,
            "source": source,
            "output": output,
            "integrated": integrated_json(&outcome),
        }));
    }
    build_local_gallery(&PipelineOptions {
        source_dir: source.clone(),
        output_dir: output.clone(),
        project_title: title.to_string(),
    })
    .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;
    Ok(json!({"published": true, "source": source, "output": output}))
}

/// Presentation-only JSON for the integrated section of `publish`. This
/// mapping is a CLI concern and intentionally lives outside the application
/// layer.
fn integrated_json(outcome: &publisher_app::PublishOutcome) -> Value {
    use publisher_app::PublicationOutcome;
    let (storage, repository, hosting) = match &outcome.publication {
        PublicationOutcome::Published(report) => (
            report.storage.as_ref().map(|report| {
                json!({
                    "uploaded": report.uploaded.len(),
                    "deleted": report.deleted.len(),
                })
            }),
            report.repository.as_ref().map(|report| {
                json!({
                    "written": report.written.len(),
                    "deleted": report.deleted.len(),
                    "revision": report.revision,
                })
            }),
            report.hosting.as_ref().map(|report| {
                json!({
                    "deployment": report.deployment.as_ref().map(|deployment| json!({
                        "id": deployment.id,
                        "url": deployment.url,
                    })),
                })
            }),
        ),
        _ => (None, None, None),
    };
    json!({
        "generation": outcome.generation,
        "storage": storage,
        "repository": repository,
        "hosting": hosting,
    })
}

fn load_project(path: &Path) -> CliResult<Value> {
    // The authoritative document loading/validation lives in the application
    // layer; the CLI preserves exactly the same classification and message.
    publisher_app::load_project_document(path).map_err(map_app_error)
}

fn map_app_error(error: publisher_app::ApplicationError) -> CliError {
    let kind = match error.kind {
        publisher_app::ApplicationErrorKind::ProjectInvalid => ErrorKind::ProjectInvalid,
        publisher_app::ApplicationErrorKind::ResourceMissing => ErrorKind::ResourceMissing,
        publisher_app::ApplicationErrorKind::Validation => ErrorKind::Validation,
        publisher_app::ApplicationErrorKind::Publication => ErrorKind::Publication,
        publisher_app::ApplicationErrorKind::Recovery => ErrorKind::Recovery,
        publisher_app::ApplicationErrorKind::Internal => ErrorKind::Internal,
    };
    CliError::new(kind, error.to_string())
}

fn required_source(project_path: &Path, project: &Value) -> CliResult<PathBuf> {
    // Identical rule as before, now owned by the application layer: the
    // matched messages and classifications are preserved unchanged.
    publisher_app::resolve_source_dir(project_path, project).map_err(map_app_error)
}

fn output_path(project_path: &Path) -> PathBuf {
    publisher_app::resolve_output_dir(project_path)
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
