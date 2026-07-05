// src/main.rs
#[macro_use]
extern crate rocket;

use dotenv::dotenv;
use rocket_okapi::openapi_get_routes;
use rocket_okapi::swagger_ui::{make_swagger_ui, SwaggerUIConfig};
use rocket_prometheus::PrometheusMetrics;
use std::sync::Arc;

mod routes;
mod auth;
mod extractors;
mod storage;
mod pypi_push;
mod publish_rabbit;
mod push_consumer;

use publish_rabbit::PublishRabbitPool;

#[tokio::main]
async fn main() {
    dotenv().ok();

    println!("Starting server...");

    let prometheus = PrometheusMetrics::new();

    // RabbitMQ publish-side pool: `routes::upload` pushes a small
    // "project@filename is ready" event here every time a client uploads an
    // allowlisted project's distribution. Connects (with retry/backoff
    // inside PublishRabbitPool::new()) before the server starts serving
    // requests, same pattern as the npm-registry service.
    println!("Connecting to RabbitMQ for pypi publish events...");
    let publish_rabbit_pool: publish_rabbit::PublishRabbitPoolRef =
        Arc::new(PublishRabbitPool::new().await);

    // Background task: consumes those events and does the actual push to
    // PyPI (or REPOSITORY_URL). Runs in-process alongside the Rocket server
    // rather than as a separate binary/pod. Replaces the retired
    // `public-registry-publisher push-py` cron job. Fire-and-forget: it
    // reconnects forever internally and never returns control to main().
    push_consumer::start_pypi_push_consumer();

    let server = rocket::build()
        .attach(prometheus.clone())
        .manage(publish_rabbit_pool)
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