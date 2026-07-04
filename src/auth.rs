use ginger_shared_rs::rocket_utils::Claims;
// src/auth.rs
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use rocket::http::{Cookie, CookieJar, SameSite, Status};
use rocket::response::Redirect;
use rocket::time::Duration;
use std::env;

pub fn validate_jwt(token: &str) -> bool {
    let secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set");
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    decode::<Claims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v).is_ok()
}

fn secure_cookies() -> bool {
    env::var("DEBUG")
        .map(|v| !matches!(v.to_lowercase().as_str(), "true" | "1"))
        .unwrap_or(true)
}

/// Checks the access_token cookie; on missing/invalid token, stores
/// `current_path` in an intended_path cookie (read back by handle_auth
/// after login) and returns a Redirect to IAM. Same behavior as
/// auth_gate() in the standalone proxy, just checked per-page here
/// instead of at a shared ingress hop.
pub fn require_auth(cookies: &CookieJar<'_>, current_path: &str) -> Result<(), Redirect> {
    let iam_login_url = env::var("IAM_LOGIN_URL").expect("IAM_LOGIN_URL must be set");

    let valid = cookies
        .get("access_token")
        .map(|c| validate_jwt(c.value()))
        .unwrap_or(false);

    if valid {
        return Ok(());
    }

    cookies.remove(Cookie::from("access_token"));
    cookies.remove(Cookie::from("refresh_token"));

    let mut intended = Cookie::new("intended_path", current_path.to_string());
    intended.set_max_age(Duration::seconds(600));
    intended.set_same_site(SameSite::Lax);
    intended.set_path("/");
    intended.set_secure(secure_cookies());
    cookies.add(intended);

    Err(Redirect::to(iam_login_url))
}

fn session_cookie(name: &str, value: String, max_age_secs: i64) -> Cookie<'static> {
    let mut c = Cookie::new(name.to_string(), value);
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(Duration::seconds(max_age_secs));
    c.set_secure(secure_cookies());
    c
}

/// GET /handle-auth/<access_token>/<refresh_token>
/// Sets session cookies, then redirects to wherever require_auth stashed
/// as intended_path (defaulting to the repo index if none was set).
#[get("/handle-auth/<access_token>/<refresh_token>")]
pub fn handle_auth(access_token: String, refresh_token: String, cookies: &CookieJar<'_>) -> Result<Redirect, Status> {
    if !validate_jwt(&access_token) {
        return Err(Status::Unauthorized);
    }

    cookies.add(session_cookie("access_token", access_token, 86400));
    cookies.add(session_cookie("refresh_token", refresh_token, 604800));

    let intended = cookies
        .get("intended_path")
        .map(|c| c.value().to_string())
        .unwrap_or_else(|| format!("/"));
    cookies.remove(Cookie::from("intended_path"));

    Ok(Redirect::to(intended))
}

#[get("/handle-auth/logout")]
pub fn logout(cookies: &CookieJar<'_>) -> Redirect {
    cookies.remove(Cookie::from("access_token"));
    cookies.remove(Cookie::from("refresh_token"));
    cookies.remove(Cookie::from("intended_path"));
    Redirect::to(format!("/"))
}