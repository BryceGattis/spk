// Copyright (c) Contributors to the SPK project.
// SPDX-License-Identifier: Apache-2.0
// https://github.com/spkenv/spk

use clap::Args;
use miette::{IntoDiagnostic, Result};
use spfs_cli_common as cli;

/// Start an spfs server
///
/// The server can be used as a remote repository by
/// its clients, communicating over gRPC and http
#[derive(Debug, Args)]
pub struct CmdServer {
    #[clap(flatten)]
    pub logging: cli::Logging,

    /// Serve a configured remote repository instead of the local one
    #[clap(long, short)]
    remote: Option<String>,

    /// The external root url that clients can use to connect to this server
    #[clap(long = "payloads-root", default_value = "http://localhost")]
    payloads_root: url::Url,

    /// The address to listen on for grpc requests
    #[clap(
        // 7737 = spfs on a dial pad
        default_value = "0.0.0.0:7737",
    )]
    grpc_address: std::net::SocketAddr,

    /// The address to listen on for http requests
    #[clap(default_value = "0.0.0.0:7787")]
    http_address: std::net::SocketAddr,
}

impl CmdServer {
    pub async fn run(&mut self, config: &spfs::Config) -> Result<i32> {
        let repo = spfs::config::open_repository_from_string(config, self.remote.as_ref()).await?;
        let repo = std::sync::Arc::new(repo);

        let payload_service =
            spfs::server::PayloadService::new(repo.clone(), self.payloads_root.clone());
        let grpc_future = tonic::transport::Server::builder()
            .add_service(spfs::server::Repository::new_srv())
            .add_service(spfs::server::TagService::new_srv(repo.clone()))
            .add_service(spfs::server::DatabaseService::new_srv(repo))
            .add_service(payload_service.clone().into_srv())
            .serve_with_shutdown(self.grpc_address, async {
                if let Err(err) = tokio::signal::ctrl_c().await {
                    tracing::error!(?err, "Failed to setup graceful shutdown handler");
                };
                tracing::info!("shutting down gRPC server...");
            });
        let http_listener = tokio::net::TcpListener::bind(self.http_address)
            .await
            .into_diagnostic()?;
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn(async move {
            if let Err(err) = tokio::signal::ctrl_c().await {
                tracing::error!(?err, "failed to setup graceful shutdown handler");
            } else {
                tracing::info!("shutting down HTTP server...");
                shutdown_tx.send(()).ok();
            }
        });
        let http_future = async move {
            loop {
                let conn = tokio::select! {
                    conn = http_listener.accept() => conn,
                    _ = &mut shutdown_rx => {
                        break;
                    }
                };
                let stream = match conn {
                    Ok((stream, _)) => {
                        tracing::debug!("Accepted connection from {:?}", stream.peer_addr());
                        stream
                    }
                    Err(err) => {
                        tracing::error!("Error accepting connection: {:?}", err);
                        continue;
                    }
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                let service = payload_service.clone();
                tokio::task::spawn(async move {
                    if let Err(err) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::error!("Error serving connection: {:?}", err);
                    }
                });
            }
            Result::<(), miette::Report>::Ok(())
        };
        tracing::info!("listening on: {}, {}", self.grpc_address, self.http_address);

        // TODO: stop the other server when one fails so that
        // the process can exit
        let (grpc_result, http_result) = tokio::join!(grpc_future, http_future);
        if let Err(err) = grpc_result {
            tracing::error!("gRPC server failed: {:?}", err);
        }
        if let Err(err) = http_result {
            tracing::error!("http server failed: {:?}", err);
        }
        Ok(0)
    }
}
