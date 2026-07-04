// src/main.rs
#[macro_use]
extern crate rocket;

use dotenv::dotenv;
use rocket_okapi::openapi_get_routes;
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use rocket_prometheus::PrometheusMetrics;

mod routes;
mod auth;
mod extractors;
mod storage;

#[tokio::main]
async fn main() {
    dotenv().ok();

    println!("Starting server...");

    let prometheus = PrometheusMetrics::new();

    let server = rocket::build()
        .attach(prometheus.clone())
        .mount("/api", openapi_get_routes![])
        .mount(
            "/api-docs",
            make_swagger_ui(&SwaggerUIConfig {
                url: "/api/openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .mount("/metrics", prometheus)
        .mount(
            "/",
            routes![
                auth::handle_auth,
                auth::logout,
                routes::simple_index,
                routes::simple_project,
                routes::download,
                routes::upload,
                routes::json_info,
                routes::package_details,
                routes::welcome,
            ],
        )
        .register("/", catchers![routes::unauthorized]);

    server.launch().await.expect("Failed to launch Rocket");
}