// src/extractors.rs
use base64::{engine::general_purpose::STANDARD, Engine as _};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use okapi::openapi3::Object;
use okapi::openapi3::SecurityRequirement;
use okapi::openapi3::SecurityScheme;
use okapi::openapi3::SecuritySchemeData;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome, Request};
use rocket_okapi::gen::OpenApiGenerator;
use rocket_okapi::request::OpenApiFromRequest;
use rocket_okapi::request::RequestHeaderInput;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;

/// Mirrors config.py's jwt_auther(): username is unused/arbitrary (pypi
/// convention is often literally the string "__token__"), and the actual
/// credential is the JWT sitting in the password field of Basic Auth.
/// Twine, pip, and any tool speaking the legacy PyPI upload API already
/// send credentials this way, so no client-side changes are needed.
pub struct BasicAuth {
    pub username: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JwtClaims {
    exp: usize,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

fn validate_jwt(token: &str) -> bool {
    let secret = match env::var("JWT_SECRET") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    decode::<JwtClaims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v).is_ok()
}

fn validate_jwt_verbose(token: &str) -> Result<(), String> {
    let secret = env::var("JWT_SECRET").map_err(|_| "JWT_SECRET_KEY not set".to_string())?;
    let mut v = Validation::new(Algorithm::HS256);
    v.validate_exp = true;
    decode::<JwtClaims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for BasicAuth {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let header = match request.headers().get_one("Authorization") {
            Some(h) => h,
            None => {
                eprintln!("[BasicAuth] no Authorization header present");
                return Outcome::Error((Status::Unauthorized, ()));
            }
        };

        let encoded = match header.strip_prefix("Basic ") {
            Some(e) => e.trim(),
            None => {
                eprintln!("[BasicAuth] Authorization header not Basic scheme: {:?}", header);
                return Outcome::Error((Status::Unauthorized, ()));
            }
        };

        let decoded = match STANDARD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("[BasicAuth] base64 decode failed: {:?}", e);
                return Outcome::Error((Status::Unauthorized, ()));
            }
        };

        let decoded_str = match String::from_utf8(decoded) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[BasicAuth] utf8 decode failed: {:?}", e);
                return Outcome::Error((Status::Unauthorized, ()));
            }
        };

        let mut parts = decoded_str.splitn(2, ':');
        let username = parts.next().unwrap_or_default().to_string();
        let password = parts.next().unwrap_or_default();

        eprintln!("[BasicAuth] username={:?} password_len={} password_prefix={:?}",
            username, password.len(), &password[..20.min(password.len())]);

        if password.is_empty() {
            eprintln!("[BasicAuth] password is empty");
            return Outcome::Error((Status::Unauthorized, ()));
        }

        match validate_jwt_verbose(password) {
            Ok(()) => Outcome::Success(BasicAuth { username }),
            Err(e) => {
                eprintln!("[BasicAuth] JWT validation failed: {}", e);
                Outcome::Error((Status::Unauthorized, ()))
            }
        }
    }
}

impl<'a> OpenApiFromRequest<'a> for BasicAuth {
    fn from_request_input(
        _gen: &mut OpenApiGenerator,
        _name: String,
        _required: bool,
    ) -> rocket_okapi::Result<RequestHeaderInput> {
        let security_scheme = SecurityScheme {
            description: Some("HTTP Basic Auth; password field is a JWT".to_owned()),
            data: SecuritySchemeData::Http {
                scheme: "basic".to_owned(),
                bearer_format: None,
            },
            extensions: Object::default(),
        };

        let mut security_req = SecurityRequirement::new();
        security_req.insert("BasicAuth".to_owned(), Vec::new());

        Ok(RequestHeaderInput::Security(
            "BasicAuth".to_owned(),
            security_scheme,
            security_req,
        ))
    }

    fn get_responses(
        _gen: &mut rocket_okapi::gen::OpenApiGenerator,
    ) -> rocket_okapi::Result<okapi::openapi3::Responses> {
        Ok(okapi::openapi3::Responses::default())
    }
}