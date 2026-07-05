//! Background consumer that mirrors newly-uploaded distributions on this
//! internal pypiserver-compatible index out to PyPI (or another
//! Warehouse-compatible index).
//!
//! Replaces the old `public-registry-publisher push-py` one-shot CLI:
//! `routes::upload` fires a small RabbitMQ event the moment a distribution
//! file lands, and this task reacts to it right away instead of waiting for
//! the next cron run.
//!
//! As with the npm push consumer, the queue message is just a pointer
//! (`filename`) — this consumer re-reads the distribution file from
//! `storage::packages_dir()` and re-derives all metadata via
//! `pypi_push::read_dist_file`, rather than trusting anything else in the
//! message body.
//!
//! Fire-and-forget from main()'s point of view: `start_pypi_push_consumer`
//! spawns this and never returns control — it reconnects to RabbitMQ forever
//! on connection loss.

use crate::publish_rabbit::PYPI_PUBLISH_EXCHANGE;
use crate::{pypi_push, storage};
use futures_util::stream::StreamExt;
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicNackOptions, BasicQosOptions,
        ExchangeDeclareOptions, QueueBindOptions, QueueDeclareOptions,
    },
    types::FieldTable,
    Connection, ConnectionProperties, ExchangeKind,
};
use serde_json::Value;
use std::time::Duration;

/// Durable queue this consumer owns. Bound to `PYPI_PUBLISH_EXCHANGE` with a
/// wildcard binding so it picks up events for every project.
const QUEUE_NAME: &str = "pypi.publish.push-to-pypi";

/// Per-message retry budget for transient failures (network blips, PyPI 5xx,
/// etc.) before the message is dead-lettered / dropped instead of requeued
/// forever.
const MAX_ATTEMPTS: u32 = 5;

/// Spawns the consumer loop. Call once from `main()` after Rocket state is
/// set up; does not block.
pub fn start_pypi_push_consumer() {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run().await {
                eprintln!("[pypi-push-consumer] connection loop ended: {:#}", e);
            }
            eprintln!("[pypi-push-consumer] reconnecting to RabbitMQ in 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn run() -> anyhow::Result<()> {
    let uri = std::env::var("RABBITMQ_URI")
        .map_err(|_| anyhow::anyhow!("RABBITMQ_URI not set"))?;

    let conn = Connection::connect(&uri, ConnectionProperties::default()).await?;
    let channel = conn.create_channel().await?;

    // Idempotent: matches the declaration in publish_rabbit.rs. Whichever
    // side starts first wins; the other just confirms the same shape.
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
        .await?;

    channel
        .queue_declare(
            QUEUE_NAME,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;

    channel
        .queue_bind(
            QUEUE_NAME,
            PYPI_PUBLISH_EXCHANGE,
            "pypi.publish.#",
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await?;

    // One in-flight message at a time: PyPI uploads already take a while
    // (hashing + multipart upload of the full file), no need to fan out yet.
    channel.basic_qos(1, BasicQosOptions::default()).await?;

    let mut consumer = channel
        .basic_consume(
            QUEUE_NAME,
            "pypi-push-consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    println!("[pypi-push-consumer] listening on '{}'", QUEUE_NAME);

    while let Some(delivery) = consumer.next().await {
        let delivery = match delivery {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[pypi-push-consumer] delivery error: {:#}", e);
                continue;
            }
        };

        match handle_message(&delivery.data).await {
            Ok(()) => {
                if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                    eprintln!("[pypi-push-consumer] ack failed: {:#}", e);
                }
            }
            Err(e) => {
                eprintln!("[pypi-push-consumer] giving up on message: {:#}", e);
                // requeue=false: handle_message already retried transient
                // failures internally, so anything reaching here is either a
                // malformed message, a corrupt/unparseable dist file, or a
                // permanent (400) rejection from PyPI. Point QUEUE_NAME at a
                // dead-letter exchange if you want to inspect these instead
                // of dropping them.
                if let Err(e) = delivery
                    .nack(BasicNackOptions {
                        requeue: false,
                        ..Default::default()
                    })
                    .await
                {
                    eprintln!("[pypi-push-consumer] nack failed: {:#}", e);
                }
            }
        }
    }

    Ok(())
}

async fn handle_message(body: &[u8]) -> anyhow::Result<()> {
    let event: Value = serde_json::from_slice(body)?;
    let filename = event
        .get("filename")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("event missing 'filename'"))?
        .to_string();

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match push_one(&filename).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS => {
                let backoff = Duration::from_secs(2u64.pow(attempt.min(5)));
                eprintln!(
                    "[pypi-push-consumer] {} attempt {}/{} failed: {:#}; retrying in {:?}",
                    filename, attempt, MAX_ATTEMPTS, e, backoff
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn push_one(filename: &str) -> anyhow::Result<()> {
    let repository_url = std::env::var("REPOSITORY_URL")
        .unwrap_or_else(|_| "https://upload.pypi.org/legacy/".to_string());
    let username = std::env::var("TWINE_USERNAME").unwrap_or_else(|_| "__token__".to_string());
    let password = std::env::var("TWINE_PASSWORD")
        .map_err(|_| anyhow::anyhow!("TWINE_PASSWORD not set, cannot push to public index"))?;

    // Guard against path traversal, same rule routes::download uses —
    // filename came off a queue message, don't trust it blindly.
    if filename.contains('/') || filename.contains('\\') {
        anyhow::bail!("refusing to push filename containing a path separator: {}", filename);
    }

    let path = storage::packages_dir().join(filename);
    if !path.is_file() {
        anyhow::bail!("distribution file '{}' missing on disk", filename);
    }

    // Parsing an sdist (tar+gzip) or wheel (zip) does synchronous file I/O
    // and decompression — run it on a blocking thread rather than tying up
    // the async runtime.
    let path_for_parse = path.clone();
    let dist = tokio::task::spawn_blocking(move || pypi_push::read_dist_file(&path_for_parse))
        .await
        .map_err(|e| anyhow::anyhow!("dist-file parse task panicked: {:#}", e))??;

    let client = reqwest::Client::new();
    match pypi_push::upload(&client, &repository_url, &username, &password, &dist).await {
        Ok(true) => {
            println!("[pypi-push-consumer] pushed {} to {}", filename, repository_url);
            Ok(())
        }
        Ok(false) => {
            println!(
                "[pypi-push-consumer] {} already exists on {}, skipping",
                filename, repository_url
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}