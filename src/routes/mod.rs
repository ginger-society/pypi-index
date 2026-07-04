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
use rocket::{Request, Response};
use std::io::Cursor;
use std::path::PathBuf;
use rocket::form::{Form, FromForm};
use rocket::http::Header;


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

#[get("/packages/<filename>")]
pub async fn download(filename: String, _auth: BasicAuth) -> Option<NamedFile> {
    let path = storage::packages_dir().join(&filename);
    // Guard against path traversal — reject any filename containing a
    // separator, since PACKAGES_DIR is intentionally flat.
    if filename.contains('/') || filename.contains('\\') {
        return None;
    }
    NamedFile::open(path).await.ok()
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
pub async fn upload(mut form: Form<UploadForm<'_>>, _auth: BasicAuth) -> Result<(), Status> {
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

    content.persist_to(&dest).await.map_err(|_| Status::InternalServerError)?;

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

    Ok(())
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