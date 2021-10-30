use std::{collections::HashMap, time::Instant};

use flate2::read::GzDecoder;
use gleam_core::{
    build::Mode,
    config::PackageConfig,
    error::{FileIoAction, FileKind},
    hex::{self, HEXPM_PUBLIC_KEY},
    io::{HttpClient as _, TarUnpacker, WrappedReader},
    paths, Error, Result,
};
use hexpm::version::{Range, Version};

use crate::{
    cli,
    fs::{self, FileSystemAccessor},
    http::HttpClient,
};

pub fn download() -> Result<()> {
    let span = tracing::info_span!("dependencies");
    let _enter = span.enter();
    let start = Instant::now();
    let mode = Mode::Dev;

    let http = HttpClient::boxed();
    let fs = FileSystemAccessor::boxed();
    let downloader = hex::Downloader::new(fs, http, Untar::boxed());

    // Read the project config
    let config = crate::config::root_config()?;
    let project_name = config.name.clone();

    // Start event loop so we can run async functions to call the Hex API
    let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");

    // Determine what versions we need
    let manifest = get_manifest(runtime.handle().clone(), mode, &config)?;

    // Remove any packages that are no longer required due to gleam.toml changes
    remove_extra_packages(&manifest)?;

    // Download them from Hex to the local cache
    cli::print_downloading("packages");
    let count =
        runtime.block_on(downloader.download_hex_packages(&manifest.packages, &project_name))?;

    // Record new state of the packages directory
    manifest.write_to_disc()?;
    LocalPackages::from_manifest(manifest).write_to_disc()?;

    // TODO: we should print the number of deps new to ./target, not to the shared cache
    cli::print_packages_downloaded(start, count);
    Ok(())
}

fn remove_extra_packages(manifest: &Manifest) -> Result<()> {
    let extra = match LocalPackages::read()? {
        Some(extra) => extra,
        None => return Ok(()),
    };
    for (package, version) in extra.extra_local_packages(manifest) {
        let path = paths::build_deps_package(&package);
        if path.exists() {
            tracing::info!(package=%package, version=%version, "removing_unneeded_package");
            fs::delete_dir(&path)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Manifest {
    requirements: HashMap<String, Range>,
    packages: HashMap<String, Version>,
}

impl Manifest {
    pub fn read_from_disc() -> Result<Self> {
        tracing::info!("Reading manifest.toml");
        let manifest_path = paths::manifest_path();
        let toml = crate::fs::read(&manifest_path)?;
        let manifest = toml::from_str(&toml).map_err(|e| Error::FileIo {
            action: FileIoAction::Parse,
            kind: FileKind::File,
            path: manifest_path.clone(),
            err: Some(e.to_string()),
        })?;
        Ok(manifest)
    }

    pub fn write_to_disc(&self) -> Result<()> {
        let path = paths::manifest_path();
        let toml = toml::to_vec(&self).expect("manifest.toml serialization");
        let mut file = fs::writer(&path)?;
        file.write(
            "# This file was generated by Gleam
# You typically do not need to edit this file manually

"
            .as_bytes(),
        )?;
        file.write(toml.as_slice())?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LocalPackages {
    packages: HashMap<String, Version>,
}

impl LocalPackages {
    pub fn extra_local_packages(&self, manifest: &Manifest) -> Vec<(String, String)> {
        let mut extra = Vec::new();
        for (name, version) in &self.packages {
            if manifest.packages.get(name.as_str()) != Some(version) {
                extra.push((name.to_string(), version.to_string()));
            }
        }
        extra
    }

    pub fn read() -> Result<Option<Self>> {
        let path = paths::packages_toml();
        if !path.exists() {
            return Ok(None);
        }
        let toml = crate::fs::read(&path)?;
        Ok(Some(toml::from_str(&toml).map_err(|e| Error::FileIo {
            action: FileIoAction::Parse,
            kind: FileKind::File,
            path: path.clone(),
            err: Some(e.to_string()),
        })?))
    }

    pub fn write_to_disc(&self) -> Result<()> {
        let path = paths::packages_toml();
        let toml = toml::to_string(&self).expect("packages.toml serialization");
        fs::write(&path, &toml)
    }

    pub fn from_manifest(manifest: Manifest) -> Self {
        Self {
            packages: manifest.packages,
        }
    }
}

#[test]
fn extra_local_packages() {
    let mut extra = LocalPackages {
        packages: vec![
            ("local1".to_string(), Version::parse("1.0.0").unwrap()),
            ("local2".to_string(), Version::parse("2.0.0").unwrap()),
            ("local3".to_string(), Version::parse("3.0.0").unwrap()),
        ]
        .into_iter()
        .collect(),
    }
    .extra_local_packages(&Manifest {
        requirements: HashMap::new(),
        packages: vec![
            ("local1".to_string(), Version::parse("1.0.0").unwrap()),
            ("local2".to_string(), Version::parse("3.0.0").unwrap()),
        ]
        .into_iter()
        .collect(),
    });
    extra.sort();
    assert_eq!(
        extra,
        vec![
            ("local2".to_string(), "2.0.0".to_string()),
            ("local3".to_string(), "3.0.0".to_string()),
        ]
    )
}

fn get_manifest(
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
) -> Result<Manifest> {
    // If there's no manifest then resolve the versions anew
    if !paths::manifest_path().exists() {
        tracing::info!("manifest_not_present");
        return resolve_versions(runtime, mode, config);
    }

    let manifest = Manifest::read_from_disc()?;

    // If the config has unchanged since the manifest was written then it is up
    // to date so we can return it unmodified.
    if manifest.requirements == config.all_dependencies()? {
        tracing::info!("manifest_up_to_date");
        Ok(manifest)
    } else {
        tracing::info!("manifest_outdated");
        // TODO: use the existing already locked versions
        resolve_versions(runtime, mode, config)
    }
}

fn resolve_versions(
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
) -> Result<Manifest, Error> {
    cli::print_resolving_versions();
    let manifest = Manifest {
        packages: hex::resolve_versions(PackageFetcher::boxed(runtime), mode, config)?,
        requirements: config.all_dependencies()?,
    };
    Ok(manifest)
}

struct PackageFetcher {
    runtime: tokio::runtime::Handle,
    http: HttpClient,
}

impl PackageFetcher {
    pub fn boxed(runtime: tokio::runtime::Handle) -> Box<Self> {
        Box::new(Self {
            runtime,
            http: HttpClient::new(),
        })
    }
}

#[derive(Debug)]
pub struct Untar;

impl Untar {
    pub fn boxed() -> Box<Self> {
        Box::new(Self)
    }
}

impl TarUnpacker for Untar {
    fn io_result_entries<'a>(
        &self,
        archive: &'a mut tar::Archive<WrappedReader>,
    ) -> std::io::Result<tar::Entries<'a, WrappedReader>> {
        archive.entries()
    }

    fn io_result_unpack(
        &self,
        path: &std::path::Path,
        mut archive: tar::Archive<GzDecoder<tar::Entry<'_, WrappedReader>>>,
    ) -> std::io::Result<()> {
        archive.unpack(path)
    }
}

impl hexpm::version::PackageFetcher for PackageFetcher {
    fn get_dependencies(
        &self,
        package: &str,
    ) -> Result<hexpm::Package, Box<dyn std::error::Error>> {
        tracing::info!(package = package, "Looking up package in Hex API");
        let config = hexpm::Config::new();
        let request = hexpm::get_package_request(package, None, &config);
        let response = self
            .runtime
            .block_on(self.http.send(request))
            .map_err(Box::new)?;
        hexpm::get_package_response(response, HEXPM_PUBLIC_KEY).map_err(|e| e.into())
    }
}
