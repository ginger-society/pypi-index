use crate::extractors::BasicAuth;
use crate::storage::{self, PkgFile};
use rocket::fs::{NamedFile, TempFile};
use rocket::http::CookieJar;
use askama::Template;

use ginger_shared_rs::rocket_utils::Claims;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket_okapi::openapi;
use serde_json::Value;

use rocket::http::{ContentType};
use rocket::response::{self, Responder};
use rocket::{Request, Response, State};
use std::io::Cursor;
use std::path::PathBuf;
use rocket::form::{Form, FromForm};
use rocket::http::Header;

use crate::publish_rabbit::{self, PublishRabbitPoolRef};
use crate::pypi_push;


pub struct Unauthorized;

impl<'r> Responder<'r, 'static> for Unauthorized {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .status(Status::Unauthorized)
            .header(Header::new("WWW-Authenticate", r#"Basic realm="pypi""#))
            .ok()
    }
}

#[catch(401)]
pub fn unauthorized() -> Unauthorized {
    Unauthorized
}

pub enum PageOrRedirect<T> {
    Page(T),
    Redirect(rocket::response::Redirect),
}

impl<'r, T: Responder<'r, 'static>> Responder<'r, 'static> for PageOrRedirect<T> {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        match self {
            PageOrRedirect::Page(t) => t.respond_to(req),
            PageOrRedirect::Redirect(r) => r.respond_to(req),
        }
    }
}

/// Projects approved to be mirrored out to the public PyPI (or another
/// Warehouse-compatible index), read from the same `SYNC_PACKAGES` env var
/// the (now-retired) `public-registry-publisher push-py` CLI used, so the
/// allowlist doesn't need to move anywhere. Re-read on every upload rather
/// than cached at startup — this is a low-frequency path and it lets the
/// allowlist be updated without restarting the index server.
///
/// Compared by exact string match against the `Name` field parsed out of
/// the distribution's own metadata. PyPI normalizes project names
/// (case-insensitive, `-`/`_`/`.` treated the same) — if your SYNC_PACKAGES
/// entries don't already match the casing/punctuation your build tool emits
/// in PKG-INFO/METADATA, normalize both sides before comparing.
fn should_sync_to_public_registry(name: &str) -> bool {
    std::env::var("SYNC_PACKAGES")
        .map(|raw| raw.split(',').map(|s| s.trim()).any(|pkg| pkg == name))
        .unwrap_or(false)
}

// ── Simple index (PEP 503) ──────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "simple_index.html")]
pub struct SimpleIndexTemplate {
    pub projects: Vec<String>,
}

impl<'r> Responder<'r, 'static> for SimpleIndexTemplate {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let html = self.render().map_err(|_| Status::InternalServerError)?;
        Response::build().header(ContentType::HTML).sized_body(html.len(), Cursor::new(html)).ok()
    }
}

#[get("/simple")]
pub fn simple_index(_auth: BasicAuth) -> SimpleIndexTemplate {
    SimpleIndexTemplate { projects: storage::get_projects() }
}
// ── Simple per-project listing ──────────────────────────────────────────────

pub struct SimpleLink {
    pub filename: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "simple_project.html")]
pub struct SimpleProjectTemplate {
    pub project: String,
    pub links: Vec<SimpleLink>,
}

impl<'r> Responder<'r, 'static> for SimpleProjectTemplate {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let html = self.render().map_err(|_| Status::InternalServerError)?;
        Response::build().header(ContentType::HTML).sized_body(html.len(), Cursor::new(html)).ok()
    }
}

#[get("/simple/<project>")]
pub fn simple_project(project: String, _auth: BasicAuth) -> Result<SimpleProjectTemplate, Status> {
    let packages = storage::find_project_packages(&project);
    if packages.is_empty() {
        return Err(Status::NotFound);
    }
    let mut links: Vec<SimpleLink> = packages
        .iter()
        .map(|p| SimpleLink {
            filename: p.relfn.clone(),
            href: format!("../../packages/{}", p.relfn),
        })
        .collect();
    links.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(SimpleProjectTemplate { project, links })
}

// ── Package download ─────────────────────────────────────────────────────────

/// Wraps a package file so it's always served as a raw binary download
/// (`application/octet-stream` + `Content-Disposition: attachment`),
/// instead of whatever Rocket's extension-based guess would produce
/// (some sdists/wheels otherwise get served as `text/plain`).
pub struct PackageDownload {
    file: NamedFile,
    filename: String,
}

impl<'r> Responder<'r, 'static> for PackageDownload {
    fn respond_to(self, req: &'r Request<'_>) -> response::Result<'static> {
        let filename = self.filename;
        Response::build_from(self.file.respond_to(req)?)
            .header(ContentType::Binary)
            .header(Header::new(
                "Content-Disposition",
                format!("attachment; filename=\"{}\"", filename),
            ))
            .ok()
    }
}

#[get("/packages/<filename>")]
pub async fn download(filename: String, _auth: BasicAuth) -> Option<PackageDownload> {
    // Guard against path traversal — reject any filename containing a
    // separator, since PACKAGES_DIR is intentionally flat.
    if filename.contains('/') || filename.contains('\\') {
        return None;
    }
    let path = storage::packages_dir().join(&filename);
    let file = NamedFile::open(path).await.ok()?;
    Some(PackageDownload { file, filename })
}

// ── Upload ────────────────────────────────────────────────────────────────────

#[derive(FromForm)]
pub struct UploadForm<'r> {
    #[field(name = ":action")]
    pub action: String,
    pub content: Option<TempFile<'r>>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub summary: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub requires_python: Option<String>,
    pub classifiers: Vec<String>,
    pub requires_dist: Vec<String>,
}

#[post("/", data = "<form>")]
pub async fn upload(
    mut form: Form<UploadForm<'_>>,
    _auth: BasicAuth,
    rabbit_pool: &State<PublishRabbitPoolRef>,
) -> Result<(), Status> {
    if form.action != "file_upload" {
        // Matches update()'s behavior for "verify"/"submit": ignored, not
        // an error. Anything else genuinely unsupported is a 400.
        return if form.action == "verify" || form.action == "submit" {
            Ok(())
        } else {
            Err(Status::BadRequest)
        };
    }

    let content = form.content.as_mut().ok_or(Status::BadRequest)?;
    let filename = content
        .raw_name()
        .map(|n| n.dangerous_unsafe_unsanitized_raw().as_str().to_string())
        .ok_or(Status::BadRequest)?;

    if filename.contains('/') || filename.contains('\\') {
        return Err(Status::BadRequest);
    }

    let dest = storage::packages_dir().join(&filename);
    if dest.exists() {
        // Matches the original's default (non --overwrite) behavior.
        return Err(Status::Conflict);
    }

    content
    .persist_to(&dest)
        .await
        .map_err(|e| {
            eprintln!("[upload] persist_to failed for {:?}: {:#}", dest, e);
            Status::InternalServerError
        })?;

    let metadata = storage::PackageMetadata {
        name: form.name.clone(),
        version: form.version.clone(),
        summary: form.summary.clone(),
        author: form.author.clone(),
        license: form.license.clone(),
        requires_python: form.requires_python.clone(),
        classifiers: if form.classifiers.is_empty() { None } else { Some(form.classifiers.clone()) },
        requires_dist: if form.requires_dist.is_empty() { None } else { Some(form.requires_dist.clone()) },
    };
    let sidecar = format!("{}.metadata.json", dest.display());
    let _ = std::fs::write(sidecar, serde_json::to_string(&metadata).unwrap_or_default());

    // Fire a "ready to push" event for the public PyPI mirror consumer, but
    // only for allowlisted projects, and only once the file (and its
    // sidecar) are safely on disk. The project name is derived from the
    // dist file's own PKG-INFO/METADATA here (the client-supplied `name`
    // form field is optional and not always trustworthy) so the
    // SYNC_PACKAGES check matches exactly what push_consumer.rs will see
    // when it re-parses the same file. Parsing runs on a blocking thread
    // since sdist/wheel parsing does synchronous file + archive I/O; a
    // parse failure here just means the event isn't queued — the upload
    // itself has already succeeded and isn't rolled back.
    let dest_for_parse = dest.clone();
    let parse_result = rocket::tokio::task::spawn_blocking(move || pypi_push::read_dist_file(&dest_for_parse))
        .await
        .map_err(|_| Status::InternalServerError)?;

    match parse_result {
        Ok(dist) => {
            if let Some(project_name) = dist.metadata.single.get("Name").cloned() {
                if should_sync_to_public_registry(&project_name) {
                    let message_body = serde_json::json!({
                        "event": "pypi_package_published",
                        "project": project_name,
                        "filename": filename,
                    })
                    .to_string();

                    let routing_key = format!("pypi.publish.{}", project_name.replace('/', "."));
                    publish_rabbit::publish_pypi_ready_event(
                        rabbit_pool.inner(),
                        &routing_key,
                        &message_body,
                    )
                    .await;
                }
            } else {
                eprintln!(
                    "[pypi-upload] parsed {} but found no 'Name' in metadata, not queuing for public push",
                    filename
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[pypi-upload] uploaded {} but failed to parse it for the public-push allowlist check: {:#}",
                filename, e
            );
        }
    }

    Ok(())
}

fn public_base_url() -> String {
    let base_url = std::env::var("PUBLIC_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:8080/".to_string());
    if base_url.ends_with('/') { base_url } else { format!("{}/", base_url) }
}

// ── JSON info (PyPI-compatible /pypi/<project>/json shape, unauthenticated) ──

#[get("/<project>/json", rank = 20)]
pub fn json_info(project: String) -> Result<Json<Value>, Status> {
    let mut packages = storage::find_project_packages(&project);
    if packages.is_empty() {
        return Err(Status::NotFound);
    }
    packages.sort_by(|a, b| a.version.cmp(&b.version));
    let latest = packages.last().unwrap();
    let meta = storage::read_metadata(latest);

    let releases: Vec<Value> = packages
        .iter()
        .map(|p| serde_json::json!({ "filename": p.relfn, "version": p.version }))
        .collect();

    Ok(Json(serde_json::json!({
        "info": {
            "name": meta.name.unwrap_or_else(|| project.clone()),
            "version": latest.version,
            "summary": meta.summary,
            "author": meta.author,
            "license": meta.license,
            "requires_python": meta.requires_python,
            "classifiers": meta.classifiers,
            "requires_dist": meta.requires_dist,
        },
        "releases": releases,
    })))
}

// ── Details page (SSR, session-cookie auth — same pattern as repo_page) ─────

pub struct ReleaseView {
    pub version: String,
    pub filename: String,
}

#[derive(Template)]
#[template(path = "details.html")]
pub struct DetailsTemplate {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub author: String,
    pub license: String,
    pub requires_python: String,
    pub classifiers: Vec<String>,
    pub requires_dist: Vec<String>,
    pub releases: Vec<ReleaseView>,
    pub index_url: String,
}

impl<'r> Responder<'r, 'static> for DetailsTemplate {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let html = self.render().map_err(|_| Status::InternalServerError)?;
        Response::build().header(ContentType::HTML).sized_body(html.len(), Cursor::new(html)).ok()
    }
}
#[get("/details/<project>")]
pub fn package_details(project: String, cookies: &CookieJar<'_>) -> PageOrRedirect<DetailsTemplate> {
    if let Err(redirect) = crate::auth::require_auth(cookies, &format!("/details/{}", project)) {
        return PageOrRedirect::Redirect(redirect);
    }

    let index_url = format!("{}simple/", public_base_url());

    let mut packages = storage::find_project_packages(&project);
    if packages.is_empty() {
        return PageOrRedirect::Page(DetailsTemplate {
            name: project,
            version: String::new(),
            summary: String::new(),
            author: String::new(),
            license: String::new(),
            requires_python: String::new(),
            classifiers: vec![],
            requires_dist: vec![],
            releases: vec![],
            index_url,
        });
    }
    packages.sort_by(|a, b| b.version.cmp(&a.version));
    let latest = packages[0].clone();
    let meta = storage::read_metadata(&latest);

    let releases = packages
        .iter()
        .map(|p| ReleaseView { version: p.version.clone(), filename: p.relfn.clone() })
        .collect();

    PageOrRedirect::Page(DetailsTemplate {
        name: meta.name.unwrap_or(project),
        version: latest.version,
        summary: meta.summary.unwrap_or_default(),
        author: meta.author.unwrap_or_default(),
        license: meta.license.unwrap_or_default(),
        requires_python: meta.requires_python.unwrap_or_default(),
        classifiers: meta.classifiers.unwrap_or_default(),
        requires_dist: meta.requires_dist.unwrap_or_default(),
        releases,
        index_url,
    })
}


#[derive(Template)]
#[template(path = "welcome.html")]
pub struct WelcomeTemplate {
    pub num_pkgs: usize,
    pub base_url: String,
    pub packages_url: String,
    pub simple_url: String,
    pub version: String,
}

impl<'r> Responder<'r, 'static> for WelcomeTemplate {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        let html = self.render().map_err(|_| Status::InternalServerError)?;
        Response::build().header(ContentType::HTML).sized_body(html.len(), Cursor::new(html)).ok()
    }
}

#[get("/")]
pub fn welcome() -> WelcomeTemplate {
    let base_url = std::env::var("PUBLIC_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:8080/".to_string());
    let base_url = if base_url.ends_with('/') { base_url } else { format!("{}/", base_url) };

    WelcomeTemplate {
        num_pkgs: storage::get_projects().len(),
        packages_url: format!("{}packages/", base_url),
        simple_url: format!("{}simple/", base_url),
        base_url,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}