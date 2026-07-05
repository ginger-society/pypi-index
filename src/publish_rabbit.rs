//! RabbitMQ publish-side pool for "distribution ready to push to PyPI"
//! events. Same shape as the npm registry's `publish_rabbit.rs` — a
//! persistent lapin connection + channel that declares its exchange once at
//! startup (retrying forever on connection failure) and exposes a
//! fire-and-forget publish function.

use lapin::{
    options::{BasicPublishOptions, ExchangeDeclareOptions},
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Topic exchange that publish events land on. `push_consumer.rs` declares
/// the same exchange (idempotent) and binds its own durable queue to it.
pub const PYPI_PUBLISH_EXCHANGE: &str = "pypi.publish";

pub struct PublishRabbitPool {
    channel: Mutex<Channel>,
}

pub type PublishRabbitPoolRef = Arc<PublishRabbitPool>;

impl PublishRabbitPool {
    /// Connects to RabbitMQ (`RABBITMQ_URI` env var), retrying with backoff
    /// forever so a slow-starting broker doesn't crash-loop the index
    /// server, then declares the durable topic exchange used for
    /// pypi-publish-ready events.
    ///
    /// Panics only if `RABBITMQ_URI` itself is missing from the
    /// environment — that's a config error, not a transient one.
    pub async fn new() -> Self {
        let uri = std::env::var("RABBITMQ_URI").expect("RABBITMQ_URI must be set");

        let mut delay = Duration::from_secs(1);
        let conn = loop {
            match Connection::connect(&uri, ConnectionProperties::default()).await {
                Ok(c) => break c,
                Err(e) => {
                    eprintln!(
                        "[pypi-publish] failed to connect to RabbitMQ ({}), retrying in {:?}",
                        e, delay
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        };

        let channel = conn
            .create_channel()
            .await
            .expect("failed to open RabbitMQ channel for pypi publish pool");

        channel
            .exchange_declare(
                PYPI_PUBLISH_EXCHANGE,
                ExchangeKind::Topic,
                ExchangeDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .expect("failed to declare pypi.publish exchange");

        println!(
            "[pypi-publish] connected to RabbitMQ, exchange '{}' ready",
            PYPI_PUBLISH_EXCHANGE
        );

        Self {
            channel: Mutex::new(channel),
        }
    }
}

/// Publishes a single "distribution ready to push" event. Fire-and-forget on
/// purpose: by the time this is called the file + metadata sidecar are
/// already durably on disk (see `routes::upload`), so a broker hiccup here
/// should log, not fail, the client's upload request. The consumer re-reads
/// from disk rather than trusting anything in the message body.
pub async fn publish_pypi_ready_event(pool: &PublishRabbitPool, routing_key: &str, message_body: &str) {
    let channel = pool.channel.lock().await;
    if let Err(e) = channel
        .basic_publish(
            PYPI_PUBLISH_EXCHANGE,
            routing_key,
            BasicPublishOptions::default(),
            message_body.as_bytes(),
            BasicProperties::default().with_delivery_mode(2), // persistent
        )
        .await
    {
        eprintln!("[pypi-publish] failed to publish event ({routing_key}): {:#}", e);
    }
}