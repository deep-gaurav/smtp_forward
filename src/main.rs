use anyhow::{Context, Result};
use tokio::net::TcpListener;

use std::env;

use smtp_forward::smtp;

/// A helper function for cleaning up old mail from the database

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let addr = format!(
        "0.0.0.0:{}",
        std::env::var("PORT").unwrap_or_else(|_| "25".into())
    );

    let domain = &std::env::var("DOMAIN").unwrap_or_else(|_| "smtp.deepwith.in".into());

    tracing::info!("edgemail server for {domain} started");

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("Listening on: {}", addr);

    // Main loop: accept connections and spawn a task to handle them
    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Accepted a connection from {}", addr);

        tokio::task::LocalSet::new()
            .run_until(async move {
                let smtp = smtp::Server::new(domain, stream).await?;
                smtp.serve().await
            })
            .await
            .ok();
    }
}
