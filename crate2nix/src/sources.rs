//! Manage nix-generated Cargo workspaces.

use crate::{
    config,
    prefetch::PrefetchableSource,
    resolve::{CratesIoSource, GitSource},
};
use anyhow::{bail, format_err, Context, Error};
use semver::Version;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};
use std::{fs::File, io::BufRead, process::Command, time::SystemTime};
use url::Url;

/// Returns the completed Source::CratesIo definition by prefetching the hash.
pub fn crates_io_source(name: String, version: Version) -> Result<config::Source, Error> {
    let prefetchable = CratesIoSource {
        name: name.clone(),
        version: version.clone(),
        sha256: None,
    };

    eprint!("Prefetching {}: ", prefetchable.to_string());
    let sha256 = prefetchable.prefetch()?;
    eprintln!("done.");

    Ok(config::Source::CratesIo {
        name,
        version,
        sha256,
    })
}

/// Returns the completed Source::Git definition by prefetching the hash.
pub fn git_io_source(url: Url, rev: String) -> Result<config::Source, Error> {
    let prefetchable = GitSource {
        url: url.clone(),
        rev: rev.clone(),
        r#ref: None,
        sha256: None,
    };

    eprint!("Prefetching {}: ", prefetchable.to_string());
    let sha256 = prefetchable.prefetch()?;
    eprintln!("done.");

    Ok(config::Source::Git { url, rev, sha256: Some(sha256) })
}

/// Operations on assmebling out-of-tree sources via nix.
pub struct FetchedSources<'a> {
    crate2nix_json_path: Cow<'a, Path>,
}

const FETCHED_SOURCES: &str = "crate2nix-sources";

impl<'a> FetchedSources<'a> {
    /// Returns a new CrateConfig for the given path.
    pub fn new<P: Into<Cow<'a, Path>>>(path: P) -> FetchedSources<'a> {
        FetchedSources {
            crate2nix_json_path: path.into(),
        }
    }

    fn project_dir(&self) -> PathBuf {
        self.crate2nix_json_path
            .parent()
            .expect("config to have parent")
            .to_path_buf()
    }

    fn sources_nix(&self) -> PathBuf {
        self.project_dir().join("crate2nix-sources.nix")
    }

    /// Create a config-nix if it doesn't exist yet.
    pub fn regenerate_sources_nix(&self) -> Result<(), Error> {
        let info = crate::GenerateInfo::default();

        if !self.crate2nix_json_path.exists() {
            bail!(
                "Did not find config at '{}'.",
                self.crate2nix_json_path.to_string_lossy()
            );
        }

        if self.sources_nix().exists() {
            let reader = std::io::BufReader::new(File::open(&self.sources_nix())?);
            let generated = reader.lines().any(|l| {
                l.map(|l| l.contains("@generated by crate2nix"))
                    .unwrap_or(false)
            });
            if !generated {
                bail!("Cowardly refusing to overwrite sources.nix without generated marker.");
            }
        }

        crate::render::SOURCES_NIX.write_to_file(&self.sources_nix(), &info)?;

        Ok(())
    }

    /// Fetches the sources via nix.
    pub fn fetch(&self) -> Result<PathBuf, Error> {
        self.regenerate_sources_nix()
            .context("while regenerating crate2nix-sources.nix")?;

        let fetched_sources_symlink = self.project_dir().join(FETCHED_SOURCES);
        download_and_link_out_of_tree_sources(
            self.project_dir(),
            &self.sources_nix(),
            &fetched_sources_symlink,
            "fetchedSources",
        )
        .context("while building crate2nix-sources directory")?;

        Ok(fetched_sources_symlink)
    }

    /// Fetches the sources via nix and returns the paths to their Cargo.tomls.
    pub fn get_cargo_tomls(&self) -> Result<Vec<PathBuf>, Error> {
        let fetched_sources_symlink = self.project_dir().join(FETCHED_SOURCES);
        let last_modified: fn(&std::path::Path) -> Option<SystemTime> = |f: &std::path::Path| {
            std::fs::symlink_metadata(f)
                .ok()
                .and_then(|m| m.modified().ok())
        };

        let has_nix_sources = {
            let config = crate::config::Config::read_from_or_default(&self.crate2nix_json_path)?;
            config
                .sources
                .values()
                .any(|s| matches!(s, config::Source::Nix { .. }))
        };
        let outdated = || {
            let symlink_generated =
                last_modified(&fetched_sources_symlink).unwrap_or(SystemTime::UNIX_EPOCH);
            let sources_modified =
                last_modified(&self.crate2nix_json_path).unwrap_or_else(SystemTime::now);
            symlink_generated < sources_modified
        };
        if has_nix_sources || outdated() {
            eprintln!("Fetching sources.");
            self.fetch()?;
        }

        let workspace_member_dir = fetched_sources_symlink;
        let mut cargo_tomls: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&workspace_member_dir).map_err(|e| {
            format_err!(
                "while iterating {} directory: {}",
                workspace_member_dir.to_string_lossy(),
                e
            )
        })? {
            let entry = entry.map_err(|e| {
                format_err!(
                    "while resolving entry in {} directory: {}",
                    workspace_member_dir.to_string_lossy().as_ref(),
                    e
                )
            })?;
            let path: PathBuf = entry.path();
            if path.is_dir() {
                let cargo_toml = path.join("Cargo.toml");
                if !cargo_toml.exists() {
                    eprintln!(
                        "WARNING: No Cargo.toml found in {}.\n\
                               This will lead to later failures.",
                        path.to_string_lossy()
                    );
                }
                let cargo_lock = path.join("Cargo.lock");
                if !cargo_lock.exists() {
                    eprintln!(
                        "WARNING: No Cargo.lock found in {}.\n\
                               This will lead to later failures.",
                        path.to_string_lossy()
                    );
                }
                cargo_tomls.push(cargo_toml);
            }
        }

        Ok(cargo_tomls)
    }
}

fn download_and_link_out_of_tree_sources(
    project_dir: impl AsRef<Path>,
    sources_nix: impl AsRef<Path>,
    generated_sources_symlink: impl AsRef<Path>,
    nix_attr: &str,
) -> Result<(), Error> {
    let project_dir = project_dir.as_ref().to_string_lossy().to_string();
    let sources_nix = sources_nix.as_ref().to_string_lossy().to_string();
    let caption = format!("Fetching sources via {} {}", sources_nix, nix_attr);
    crate::command::run(
        &caption,
        Command::new("nix").current_dir(&project_dir).args(&[
            "--show-trace",
            "build",
            "-f",
            &sources_nix,
            nix_attr,
            "-o",
            generated_sources_symlink
                .as_ref()
                .to_string_lossy()
                .as_ref(),
        ]),
    )?;

    Ok(())
}
