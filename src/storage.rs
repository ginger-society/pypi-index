// src/storage.rs
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PkgFile {
    pub pkgname: String,
    pub pkgname_norm: String,
    pub version: String,
    pub relfn: String, // filename relative to PACKAGES_DIR
    pub full_path: PathBuf,
}

static WHEEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?x)^(?P<name>.+?)-(?P<ver>\d[^-]*?)(-(?P<build>\d[^-]*?))?-(?P<pyver>[^-]+)-(?P<abi>[^-]+)-(?P<plat>[^-]+)\.whl$").unwrap()
});

static ARCHIVE_SUFFIX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\.zip|\.tar\.gz|\.tgz|\.tar\.bz2|\.tar\.xz|-py[23]\.\d-.*|\.egg)$").unwrap()
});

static DASH_DIGIT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-(\d)").unwrap());
static NORMALIZE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[-_.]+").unwrap());

/// PEP 503 normalization.
pub fn normalize_pkgname(name: &str) -> String {
    NORMALIZE_RE.replace_all(name, "-").to_lowercase()
}

/// Port of guess_pkgname_and_version() from pkg_helpers.py. Handles wheels
/// exactly (structured filename per the wheel spec) and falls back to a
/// simpler dash-then-digit heuristic for sdists, matching the original's
/// intent closely enough for typical filenames without reimplementing its
/// full edge-case regex chain.
pub fn guess_pkgname_and_version(filename: &str) -> Option<(String, String)> {
    if filename.ends_with(".whl") {
        let caps = WHEEL_RE.captures(filename)?;
        let name = caps.name("name")?.as_str().to_string();
        let ver = caps.name("ver")?.as_str().to_string();
        let version = match caps.name("build") {
            Some(b) => format!("{}-{}", ver, b.as_str()),
            None => ver,
        };
        return Some((name, version));
    }

    if !ARCHIVE_SUFFIX_RE.is_match(filename) {
        return None;
    }
    let stripped = ARCHIVE_SUFFIX_RE.replace(filename, "").to_string();

    if !stripped.contains('-') {
        return Some((stripped, String::new()));
    }
    if stripped.matches('-').count() == 1 {
        let mut parts = stripped.splitn(2, '-');
        let name = parts.next().unwrap_or_default().to_string();
        let version = parts.next().unwrap_or_default().to_string();
        return Some((name, version));
    }
    if let Some(m) = DASH_DIGIT_RE.find(&stripped) {
        let idx = m.start();
        return Some((stripped[..idx].to_string(), stripped[idx + 1..].to_string()));
    }
    Some((stripped, String::new()))
}

pub fn packages_dir() -> PathBuf {
    PathBuf::from(std::env::var("PACKAGES_DIR").unwrap_or_else(|_| "/data/packages".to_string()))
}

/// Flat scan of PACKAGES_DIR — matches the layout your publisher binary
/// already writes to and reads from (find_py_matches in common.rs), so no
/// recursive directory walking is needed here.
pub fn scan_packages() -> Vec<PkgFile> {
    let root = packages_dir();
    let mut out = Vec::new();

    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let fname = match path.file_name().and_then(|f| f.to_str()) {
            Some(f) => f,
            None => continue,
        };
        if fname.ends_with(".metadata.json") || fname.starts_with('.') {
            continue;
        }
        if let Some((pkgname, version)) = guess_pkgname_and_version(fname) {
            out.push(PkgFile {
                pkgname_norm: normalize_pkgname(&pkgname),
                pkgname,
                version,
                relfn: fname.to_string(),
                full_path: path.clone(),
            });
        }
    }
    out
}

pub fn find_project_packages(project: &str) -> Vec<PkgFile> {
    let normalized = normalize_pkgname(project);
    scan_packages()
        .into_iter()
        .filter(|p| p.pkgname_norm == normalized)
        .collect()
}

pub fn get_projects() -> Vec<String> {
    let mut names: Vec<String> = scan_packages()
        .into_iter()
        .map(|p| p.pkgname_norm)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    names.sort();
    names
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct PackageMetadata {
    pub name: Option<String>,
    pub version: Option<String>,
    pub summary: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub requires_python: Option<String>,
    pub classifiers: Option<Vec<String>>,
    pub requires_dist: Option<Vec<String>>,
}

/// Reads the `.metadata.json` sidecar written at upload time (see upload()
/// in routes.rs) — same sidecar-file convention as the original Python's
/// json_info()/package_details().
pub fn read_metadata(pkg: &PkgFile) -> PackageMetadata {
    let meta_path = pkg.full_path.with_extension(
        format!("{}.metadata.json", pkg.full_path.extension().and_then(|e| e.to_str()).unwrap_or("")),
    );
    // with_extension mangles multi-dot filenames (e.g. tar.gz), so build the
    // sidecar path directly instead, matching Python's `relfn + ".metadata.json"`.
    let meta_path = PathBuf::from(format!("{}.metadata.json", pkg.full_path.display()));
    let _ = meta_path; // shadow warning guard if the with_extension line above is removed later

    let sidecar = PathBuf::from(format!("{}.metadata.json", pkg.full_path.display()));
    match fs::read_to_string(&sidecar) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => PackageMetadata::default(),
    }
}