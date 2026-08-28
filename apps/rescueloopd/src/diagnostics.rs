use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use crate::{console, doctor, logging, storage};

const BUNDLE_SCHEMA_VERSION: u16 = 1;
const MAX_LOG_FILES: usize = 4;
const MAX_LOG_FILE_INPUT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LOG_RECORDS: usize = 200;
const MAX_LOG_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_MEMBER_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Serialize)]
struct Manifest {
    schema_version: u16,
    generated_at: String,
    rescueloop_version: &'static str,
    platform: Platform,
    privacy: Privacy,
    bounds: Bounds,
}

#[derive(Serialize)]
struct Platform {
    os: &'static str,
    architecture: &'static str,
}

#[derive(Serialize)]
struct Privacy {
    redacted: bool,
    excluded: [&'static str; 6],
}

#[derive(Serialize)]
struct Bounds {
    recent_log_records: usize,
    recent_log_bytes: usize,
    archive_content_bytes: usize,
    archive_bytes: usize,
}

#[derive(Serialize)]
struct SafeConfiguration {
    settings_readable: bool,
    enabled_sources: Vec<String>,
    analysis_provider_configured: bool,
    otlp_log_export_enabled: bool,
    otlp_trace_export_enabled: bool,
}

#[derive(Serialize)]
struct IntegrityResults<'a> {
    incident_json: Option<&'a doctor::Check>,
    sqlite_projection: Option<&'a doctor::Check>,
    lineage_ledger: Option<&'a doctor::Check>,
    local_slo_assertions: &'a [crate::slo::Assertion],
}

struct Member {
    name: &'static str,
    description: &'static str,
    bytes: Vec<u8>,
}

struct PreparedBundle {
    members: Vec<Member>,
    log_records: usize,
}

pub async fn export(
    incident_dir: &Path,
    logs: &logging::LogGuard,
    output: Option<PathBuf>,
    confirm: bool,
) -> Result<()> {
    let prepared = prepare(incident_dir, logs).await?;
    print_preview(&prepared);
    std::io::stdout().flush()?;
    if !confirm {
        println!("\nPreview only: no archive was written.");
        println!("Repeat with --confirm to write this exact bounded bundle shape.");
        return Ok(());
    }

    let output = output.unwrap_or_else(default_output);
    let output = if output.is_absolute() {
        output
    } else {
        std::env::current_dir()?.join(output)
    };
    validate_output(&output)?;
    let archive = build_archive(&prepared.members)?;
    if archive.len() > MAX_BUNDLE_BYTES {
        bail!(
            "diagnostic archive exceeded its {} byte bound",
            MAX_BUNDLE_BYTES
        );
    }
    if !storage::create_durable(&output, &archive).await? {
        bail!("diagnostic output already exists; refusing to overwrite it");
    }
    println!("\nDiagnostic bundle written: {}", output.display());
    println!("Archive size: {} bytes", archive.len());
    Ok(())
}

async fn prepare(incident_dir: &Path, logs: &logging::LogGuard) -> Result<PreparedBundle> {
    let health = doctor::collect(incident_dir, logs).await;
    let settings = load_bounded_settings(incident_dir).await;
    let (recent_logs, log_records) = recent_redacted_logs(incident_dir)?;
    let configuration = SafeConfiguration {
        settings_readable: settings.is_ok(),
        enabled_sources: settings.map_or_else(|_| Vec::new(), |settings| settings.enabled_sources),
        analysis_provider_configured: provider_configured(incident_dir).await?,
        otlp_log_export_enabled: configured_env("RESCUELOOP_OTLP_ENDPOINT"),
        otlp_trace_export_enabled: configured_env("RESCUELOOP_OTLP_TRACES_ENDPOINT"),
    };
    let integrity = IntegrityResults {
        incident_json: find_check(&health, "incident store"),
        sqlite_projection: find_check(&health, "SQLite projection"),
        lineage_ledger: find_check(&health, "lineage ledger"),
        local_slo_assertions: &health.slo_assertions,
    };
    let manifest = Manifest {
        schema_version: BUNDLE_SCHEMA_VERSION,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        rescueloop_version: env!("CARGO_PKG_VERSION"),
        platform: Platform {
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
        },
        privacy: Privacy {
            redacted: true,
            excluded: [
                "incident evidence",
                "filesystem paths",
                "launch arguments",
                "tokens and secrets",
                "model payloads",
                "repair contents",
            ],
        },
        bounds: Bounds {
            recent_log_records: MAX_LOG_RECORDS,
            recent_log_bytes: MAX_LOG_OUTPUT_BYTES,
            archive_content_bytes: MAX_ARCHIVE_CONTENT_BYTES,
            archive_bytes: MAX_BUNDLE_BYTES,
        },
    };

    let members = vec![
        json_member(
            "manifest.json",
            "version, platform, privacy policy, and bounds",
            &manifest,
        )?,
        json_member("health.json", "bounded doctor health snapshot", &health)?,
        json_member(
            "metrics.json",
            "typed local metrics snapshot",
            &health.metrics,
        )?,
        json_member(
            "event-sources.json",
            "event-source status and bounded counters",
            &health.sources,
        )?,
        json_member(
            "integrity.json",
            "JSON, SQLite, ledger, and local SLO results",
            &integrity,
        )?,
        json_member(
            "configuration.json",
            "allowlisted configuration without values that identify local resources",
            &configuration,
        )?,
        Member {
            name: "recent-logs.jsonl",
            description: "recent structured logs, redacted again for support export",
            bytes: recent_logs,
        },
    ];
    for member in &members {
        if member.bytes.len() > MAX_MEMBER_BYTES {
            bail!("{} exceeded its diagnostic member bound", member.name);
        }
    }
    Ok(PreparedBundle {
        members,
        log_records,
    })
}

fn print_preview(bundle: &PreparedBundle) {
    println!("RescueLoop diagnostic bundle preview");
    println!("Nothing has been written yet.\n");
    println!("INCLUDED");
    for member in &bundle.members {
        println!(
            "  {:<24} {:>8} bytes  {}",
            member.name,
            member.bytes.len(),
            member.description
        );
    }
    println!(
        "\nRecent log records: {}/{}",
        bundle.log_records, MAX_LOG_RECORDS
    );
    println!("Maximum archive size: {} bytes", MAX_BUNDLE_BYTES);
    println!("\nEXCLUDED");
    println!(
        "  incident evidence, paths, launch arguments, secrets, model payloads, repair contents"
    );
}

fn json_member(
    name: &'static str,
    description: &'static str,
    value: &impl Serialize,
) -> Result<Member> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(Member {
        name,
        description,
        bytes,
    })
}

fn build_archive(members: &[Member]) -> Result<Vec<u8>> {
    let content_bytes = members
        .iter()
        .try_fold(0_usize, |total, member| {
            total.checked_add(member.bytes.len())
        })
        .context("diagnostic archive content size overflow")?;
    if content_bytes > MAX_ARCHIVE_CONTENT_BYTES {
        bail!(
            "diagnostic archive content exceeded its {} byte bound",
            MAX_ARCHIVE_CONTENT_BYTES
        );
    }
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for member in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(member.bytes.len() as u64);
        header.set_mode(0o600);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, member.name, Cursor::new(&member.bytes))?;
    }
    let encoder = archive.into_inner()?;
    Ok(encoder.finish()?)
}

fn recent_redacted_logs(incident_dir: &Path) -> Result<(Vec<u8>, usize)> {
    let directory = logging::log_directory(incident_dir);
    let mut paths = if directory.exists() {
        std::fs::read_dir(directory)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_file())
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("rescueloop-")
                    && matches!(
                        entry.path().extension().and_then(|value| value.to_str()),
                        Some("jsonl" | "gz")
                    )
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    paths.sort();
    let mut records = VecDeque::with_capacity(MAX_LOG_RECORDS);
    for path in paths.into_iter().rev().take(MAX_LOG_FILES).rev() {
        let bytes = read_log_tail(&path)?;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            redact_for_bundle(&mut value, None);
            let encoded = serde_json::to_vec(&value)?;
            if encoded.len() > MAX_LOG_OUTPUT_BYTES {
                continue;
            }
            while records.len() == MAX_LOG_RECORDS {
                records.pop_front();
            }
            records.push_back(encoded);
        }
    }
    let mut selected = VecDeque::new();
    let mut selected_bytes = 0_usize;
    for record in records.iter().rev() {
        let next = selected_bytes
            .saturating_add(record.len())
            .saturating_add(1);
        if next > MAX_LOG_OUTPUT_BYTES {
            continue;
        }
        selected.push_front(record);
        selected_bytes = next;
    }
    let mut output = Vec::with_capacity(selected_bytes);
    for record in &selected {
        output.extend_from_slice(record);
        output.push(b'\n');
    }
    Ok((output, selected.len()))
}

fn read_log_tail(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    if path.extension().is_some_and(|value| value == "gz") {
        if file.metadata()?.len() > MAX_LOG_FILE_INPUT_BYTES {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        GzDecoder::new(file)
            .take(MAX_LOG_FILE_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        return Ok(if bytes.len() as u64 > MAX_LOG_FILE_INPUT_BYTES {
            Vec::new()
        } else {
            bytes
        });
    }
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(MAX_LOG_FILE_INPUT_BYTES);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if offset > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(bytes)
}

fn redact_for_bundle(value: &mut Value, key: Option<&str>) {
    if key.is_some_and(sensitive_key) {
        *value = Value::String("[REDACTED]".into());
        return;
    }
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                redact_for_bundle(value, Some(key));
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_for_bundle(value, key);
            }
        }
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "password",
        "secret",
        "authorization",
        "bearer",
        "argument",
        "command",
        "evidence",
        "payload",
        "content",
        "endpoint",
        "url",
        "uri",
        "header",
        "cookie",
    ]
    .iter()
    .any(|needle| key.contains(needle))
        || matches!(key.as_str(), "path" | "directory" | "artifact")
        || key.ends_with("_path")
}

fn redact_text(text: &str) -> String {
    text.split_whitespace()
        .map(|part| {
            let trimmed = part.trim_matches(|character: char| "'\"()[]{}<>,.;".contains(character));
            let assigned = trimmed.split_once('=').map_or(trimmed, |(_, value)| value);
            if is_platform_neutral_absolute(assigned) || assigned.starts_with("file://") {
                "<PATH>"
            } else if assigned.contains("://") {
                "<URL>"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_platform_neutral_absolute(value: &str) -> bool {
    value.starts_with('/') || is_windows_absolute(value)
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn find_check<'a>(snapshot: &'a doctor::DoctorSnapshot, name: &str) -> Option<&'a doctor::Check> {
    snapshot.checks.iter().find(|check| check.name == name)
}

async fn provider_configured(incident_dir: &Path) -> Result<bool> {
    let path = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("config.json");
    Ok(tokio::fs::try_exists(path).await?)
}

async fn load_bounded_settings(incident_dir: &Path) -> Result<console::Settings> {
    let path = incident_dir
        .parent()
        .unwrap_or(incident_dir)
        .join("settings.json");
    tokio::task::spawn_blocking(move || {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(console::Settings::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            bail!("settings exceed the diagnostic read bound");
        }
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
}

fn configured_env(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn default_output() -> PathBuf {
    PathBuf::from(format!(
        "rescueloop-diagnostics-{}.tar.gz",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ))
}

fn validate_output(path: &Path) -> Result<()> {
    if path.file_name().is_none() || path.is_dir() {
        bail!("diagnostic output must name a file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn bundle_redaction_removes_sensitive_keys_and_embedded_paths() {
        let mut value = serde_json::json!({
            "token": "probe-secret",
            "arguments": ["--token", "probe-secret"],
            "message": "failed at /Users/probe/private/file.txt",
            "windows": "failed at C:\\Users\\probe\\private.txt",
            "assigned": "path=/private/probe/file.txt",
            "remote": "endpoint=https://private.example.invalid/token",
            "safe": "source retrying"
        });
        redact_for_bundle(&mut value, None);
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains("probe-secret"));
        assert!(!encoded.contains("/Users/probe"));
        assert!(!encoded.contains("C:\\\\Users"));
        assert!(!encoded.contains("private.example.invalid"));
        assert!(encoded.contains("source retrying"));
    }

    #[test]
    fn archive_has_only_fixed_safe_member_names() {
        let members = vec![Member {
            name: "manifest.json",
            description: "fixture",
            bytes: b"{}".to_vec(),
        }];
        let bytes = build_archive(&members).unwrap();
        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        let names = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![PathBuf::from("manifest.json")]);
    }

    #[test]
    fn recent_logs_are_bounded_and_redacted_for_support() {
        let root = std::env::temp_dir().join(format!("rescueloop-diagnostics-{}", Uuid::new_v4()));
        let incident_dir = root.join("incidents");
        let log_dir = root.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let path = log_dir.join(format!(
            "rescueloop-2026-08-28-{}-0000.jsonl",
            Uuid::new_v4()
        ));
        let records = (0..MAX_LOG_RECORDS + 10)
            .map(|index| {
                serde_json::json!({
                    "timestamp": "2026-08-28T12:00:00Z",
                    "sequence": index,
                    "token": "diagnostic-secret-sentinel",
                    "fields": {
                        "event": "fixture",
                        "message": format!("failed at /Users/private/{index}")
                    }
                })
            })
            .map(|value| serde_json::to_string(&value).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, records).unwrap();

        let (encoded, count) = recent_redacted_logs(&incident_dir).unwrap();
        let text = String::from_utf8(encoded).unwrap();
        assert_eq!(count, MAX_LOG_RECORDS);
        assert!(text.len() <= MAX_LOG_OUTPUT_BYTES);
        assert!(!text.contains("diagnostic-secret-sentinel"));
        assert!(!text.contains("/Users/private"));
        assert!(!text.contains("\"sequence\":0"));
        assert!(text.contains(&format!("\"sequence\":{}", MAX_LOG_RECORDS + 9)));
    }

    #[tokio::test]
    async fn settings_reader_rejects_unbounded_configuration() {
        let root = std::env::temp_dir().join(format!("rescueloop-settings-{}", Uuid::new_v4()));
        let incident_dir = root.join("incidents");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("settings.json"),
            vec![b'x'; MAX_SETTINGS_BYTES as usize + 1],
        )
        .unwrap();
        assert!(load_bounded_settings(&incident_dir).await.is_err());
    }
}
