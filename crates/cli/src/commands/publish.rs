use anyhow::{anyhow, Result};
use butterflow_core::utils;
use butterflow_models::workflow;
use clap::Args;
use log::{debug, info, warn};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_yaml;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::auth::TokenStorage;
use codemod_telemetry::send_event::{BaseEvent, TelemetrySender};
use crate::commands::workflow::validate::validate_codemod_manifest_structure;

#[derive(Args, Debug)]
pub struct Command {
    /// Path to codemod directory
    path: Option<PathBuf>,
    /// Explicit version override
    #[arg(long)]
    version: Option<String>,
    /// Target registry URL
    #[arg(long)]
    registry: Option<String>,
    /// Tag for the release
    #[arg(long)]
    tag: Option<String>,
    /// Access level (public, private)
    #[arg(long)]
    access: Option<String>,
    /// Validate and pack without uploading
    #[arg(long)]
    dry_run: bool,
}

#[derive(Deserialize, Serialize, Debug)]
struct CodemodManifest {
    schema_version: String,
    name: String,
    version: String,
    description: String,
    author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copyright: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bugs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    registry: Option<RegistryConfig>,
    workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    targets: Option<TargetConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keywords: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    readme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changelog: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation: Option<ValidationConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
struct RegistryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    access: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
struct TargetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    languages: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frameworks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    versions: Option<std::collections::HashMap<String, String>>,
}

#[derive(Deserialize, Serialize, Debug)]
struct ValidationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    require_tests: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_test_coverage: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct PublishResponse {
    success: bool,
    package: PublishedPackage,
}

#[derive(Deserialize, Debug)]
struct PublishedPackage {
    #[allow(dead_code)]
    id: String,
    name: String,
    version: String,
    scope: Option<String>,
    download_url: String,
    published_at: String,
}

pub async fn handler(args: &Command, telemetry: &dyn TelemetrySender) -> Result<()> {
    let package_path = args
        .path
        .as_ref()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    info!("Publishing codemod from: {}", package_path.display());

    // Load and validate manifest
    let mut manifest = load_manifest(&package_path)?;

    // Override version if specified
    if let Some(version) = &args.version {
        manifest.version = version.clone();
    }

    // Override access if specified
    if let Some(access) = &args.access {
        if manifest.registry.is_none() {
            manifest.registry = Some(RegistryConfig {
                access: Some(access.clone()),
                scope: None,
                visibility: None,
            });
        } else if let Some(ref mut registry) = manifest.registry {
            registry.access = Some(access.clone());
        }
    }

    // Validate codemod manifest structure
    validate_codemod_manifest_structure(&package_path, &manifest)?;

    // Create codemod tarball
    let tarball_path = create_codemod_tarball(&package_path, &manifest, args.dry_run)?;

    if args.dry_run {
        println!("✓ Package validation successful");
        println!(
            "✓ tarball created: {} ({} bytes)",
            tarball_path.display(),
            fs::metadata(&tarball_path)?.len()
        );
        println!("📦 Package ready for publishing");
        return Ok(());
    }

    // Get registry configuration
    let storage = TokenStorage::new()?;
    let config = storage.load_config()?;
    let registry_url = args
        .registry
        .as_ref()
        .unwrap_or(&config.default_registry)
        .clone();

    // Check authentication
    let auth = storage
        .get_auth_for_registry(&registry_url)?
        .ok_or_else(|| {
            anyhow!(
                "Not authenticated with registry: {}. Run 'codemod login' first.",
                registry_url
            )
        })?;

    // Upload package
    let response = upload_codemod(
        &registry_url,
        &bundle_path,
        &manifest,
        &auth.tokens.access_token,
    )
    .await?;

    if !response.success {
        return Err(anyhow!("Failed to publish package"));
    }

    let cli_version = env!("CARGO_PKG_VERSION");

    let _ = telemetry
        .send_event(
            BaseEvent {
                kind: "codemodPublished".to_string(),
                properties: HashMap::from([
                    ("codemodName".to_string(), manifest.name.clone()),
                    ("version".to_string(), manifest.version.clone()),
                    ("cliVersion".to_string(), cli_version.to_string()),
                ]),
            },
            None,
        )
        .await;

    println!("✅ Package published successfully!");
    println!("📦 {}", format_codemod_name(&response.package));
    println!("🏷️  Version: {}", response.package.version);
    println!("📅 Published: {}", response.package.published_at);
    println!("🔗 Download: {}", response.package.download_url);

    // Clean up temporary bundle
    if let Err(e) = fs::remove_file(&bundle_path) {
        warn!("Failed to clean up temporary bundle: {e}");
    }

    Ok(())
}

fn load_manifest(package_path: &Path) -> Result<CodemodManifest> {
    let manifest_path = package_path.join("codemod.yaml");

    if !manifest_path.exists() {
        return Err(anyhow!(
            "codemod.yaml not found in {}",
            package_path.display()
        ));
    }

    let manifest_content = fs::read_to_string(&manifest_path)?;
    let manifest: CodemodManifest = serde_yaml::from_str(&manifest_content)
        .map_err(|e| anyhow!("Failed to parse codemod.yaml: {}", e))?;

    debug!(
        "Loaded manifest for package: {} v{}",
        manifest.name, manifest.version
    );
    Ok(manifest)
}

fn create_codemod_tarball(
    package_path: &Path,
    manifest: &CodemodManifest,
    dry_run: bool,
) -> Result<PathBuf> {
    let temp_dir = TempDir::new()?;
    let bundle_name = format!("{}-{}.tar.gz", manifest.name, manifest.version);
    let temp_bundle_path = temp_dir.path().join(&bundle_name);

    // Create tar.gz archive
    let tar_gz = fs::File::create(&temp_bundle_path)?;
    let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    // Add files to archive
    let mut file_count = 0;
    for entry in WalkDir::new(package_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path() != package_path) // Skip the root directory itself
        .filter(|e| should_include_file(e.path(), package_path))
    {
        if entry.file_type().is_file() {
            let relative_path = entry.path().strip_prefix(package_path)?;
            debug!("Adding file to bundle: {}", relative_path.display());
            tar.append_path_with_name(entry.path(), relative_path)?;
            file_count += 1;
        }
    }

    info!("Added {file_count} files to bundle");

    // Finish the tar archive and flush the gzip encoder
    let enc = tar.into_inner()?;
    enc.finish()?;

    let bundle_size = fs::metadata(&temp_bundle_path)?.len();
    const MAX_BUNDLE_SIZE: u64 = 10 * 1024 * 1024; // 10MB compressed

    if bundle_size > MAX_BUNDLE_SIZE {
        return Err(anyhow!(
            "Compressed bundle too large: {} bytes. Maximum allowed: {} bytes.",
            bundle_size,
            MAX_BUNDLE_SIZE
        ));
    }

    info!("Created codemod tarball: {bundle_name} ({bundle_size} bytes)");

    // Move to a persistent location (both dry-run and regular publishing)
    let output_path = if dry_run {
        std::env::current_dir()?.join(&bundle_name)
    } else {
        // Create a temporary file in the system temp directory that won't be auto-cleaned
        let system_temp = std::env::temp_dir();
        system_temp.join(&bundle_name)
    };

    fs::copy(&temp_bundle_path, &output_path)?;
    Ok(output_path)
}

fn should_include_file(file_path: &Path, package_root: &Path) -> bool {
    let relative_path = match file_path.strip_prefix(package_root) {
        Ok(path) => path,
        Err(_) => {
            debug!("Failed to strip prefix for: {}", file_path.display());
            return false;
        }
    };

    let path_str = relative_path.to_string_lossy();

    // Exclude common development/build artifacts
    const EXCLUDED_PATTERNS: &[&str] = &[
        ".git/",
        ".gitignore",
        "node_modules/",
        "target/",
        ".cargo/",
        "__pycache__/",
        "*.pyc",
        ".venv/",
        ".env",
        ".DS_Store",
        "Thumbs.db",
    ];

    for pattern in EXCLUDED_PATTERNS {
        if pattern.ends_with('/') {
            if path_str.starts_with(pattern) {
                debug!("Excluding directory: {path_str} (matches {pattern})");
                return false;
            }
        } else if pattern.contains('*') {
            // Simple glob matching
            if *pattern == "*.pyc" && path_str.ends_with(".pyc") {
                debug!("Excluding file: {path_str} (matches {pattern})");
                return false;
            }
        } else if path_str == *pattern {
            debug!("Excluding file: {path_str} (matches {pattern})");
            return false;
        }
    }

    debug!("Including file: {path_str}");
    true
}

async fn upload_codemod(
    registry_url: &str,
    tarball_path: &Path,
    manifest: &CodemodManifest,
    access_token: &str,
) -> Result<PublishResponse> {
    let client = reqwest::Client::new();

    let codemod_name = if let Some(registry) = &manifest.registry {
        if let Some(scope) = &registry.scope {
            format!("{}/{}", scope, manifest.name)
        } else {
            manifest.name.clone()
        }
    } else {
        manifest.name.clone()
    };

    let url = format!("{registry_url}/api/v1/registry/packages/{codemod_name}");

    // Read tarball file
    let tarball_data = fs::read(tarball_path)?;
    let manifest_json = serde_json::to_string(manifest)?;

    // Create multipart form
    let form = reqwest::multipart::Form::new()
        .part(
            "packageFile",
            reqwest::multipart::Part::bytes(bundle_data)
                .file_name(format!("{}-{}.tar.gz", manifest.name, manifest.version))
                .mime_str("application/gzip")?,
        )
        .text("manifest", manifest_json);

    debug!("Uploading to: {url}");

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "codemod-cli/1.0")
        .multipart(form)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::CONFLICT {
            return Err(anyhow!("Version {} already exists.", manifest.version));
        } else if status == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow!(
                "Access denied. You may not have permission to publish to this package."
            ));
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "Authentication failed. Please run 'codemod login' again."
            ));
        }

        return Err(anyhow!("Upload failed ({}): {}", status, error_text));
    }

    let publish_response: PublishResponse = response.json().await?;
    Ok(publish_response)
}


fn format_codemod_name(package: &PublishedPackage) -> String {
    if let Some(scope) = &package.scope {
        format!("{}/{}", scope, package.name)
    } else {
        package.name.clone()
    }
}
