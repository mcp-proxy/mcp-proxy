use anyhow::Result;
use clap::Parser;
use mcp_proxy::r#static::{StaticConfig, run_local_client};
use mcp_proxy::xds::Config as XdsConfig;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing_subscriber::{self, EnvFilter};

use mcp_proxy::admin::App as AdminApp;
use mcp_proxy::inbound;
use mcp_proxy::metrics::App as MetricsApp;
use mcp_proxy::proto::mcpproxy::dev::rbac::Config as XdsRbac;
use mcp_proxy::proto::mcpproxy::dev::target::Target as XdsTarget;
use mcp_proxy::relay;
use mcp_proxy::signal;
use mcp_proxy::xds;
use mcp_proxy::xds::ProxyStateUpdater;
use mcp_proxy::xds::XdsStore as ProxyState;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
	/// Use config from bytes
	#[arg(short, long, value_name = "config")]
	config: Option<bytes::Bytes>,

	/// Use config from file
	#[arg(short, long, value_name = "file")]
	file: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Config {
	#[serde(rename = "static")]
	Static(StaticConfig),
	#[serde(rename = "xds")]
	Xds(XdsConfig),
}

#[tokio::main]
async fn main() -> Result<()> {
	// Initialize logging
	// Initialize the tracing subscriber with file and stdout logging
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
		.with_writer(std::io::stderr)
		.with_ansi(false)
		.init();

	let mut registry = Registry::default();

	let args = Args::parse();

	// TODO: Do this better
	rustls::crypto::ring::default_provider()
		.install_default()
		.expect("failed to install ring provider");

	let cfg: Config = match (args.file, args.config) {
		(Some(filename), None) => {
			// If filename is a URL, download it
			match reqwest::Url::parse(&filename) {
				Ok(url) => {
					println!("Downloading config from URL: {}", url);
					let response = reqwest::get(url).await?;
					let body = response.text().await?;
					serde_json::from_str(&body)?
				},
				Err(_) => {
					println!("Reading config from file: {}", filename);
					let file = tokio::fs::read_to_string(filename).await?;
					serde_json::from_str(&file)?
				},
			}
		},
		(None, Some(config)) => {
			let file = std::str::from_utf8(&config).map(|s| s.to_string())?;
			serde_json::from_str(&file)?
		},
		(Some(_), Some(_)) => {
			eprintln!("config error: both --file and --config cannot be provided, exiting");
			std::process::exit(1);
		},
		(None, None) => {
			eprintln!("Error: either --file or --config must be provided, exiting");
			std::process::exit(1);
		},
	};

	let ct = tokio_util::sync::CancellationToken::new();
	let ct_clone = ct.clone();
	tokio::spawn(async move {
		let sig = signal::Shutdown::new();
		sig.wait().await;
		ct_clone.cancel();
	});

	match cfg {
		Config::Static(cfg) => {
			let mut run_set = JoinSet::new();

			let cfg_clone = cfg.clone();
			let listener = inbound::Listener::from_xds(cfg_clone.listener.clone())
				.await
				.unwrap();
			let state = Arc::new(tokio::sync::RwLock::new(ProxyState::new(listener)));

			let relay_metrics = relay::metrics::Metrics::new(&mut registry);

			let state_2 = state.clone();
			let cfg_clone = cfg.clone();
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				run_local_client(&cfg_clone, state_2, Arc::new(relay_metrics), ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error running local client: {:?}", e))
			});

			// Add metrics listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				start_metrics_service(Arc::new(registry), ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving metrics: {:?}", e))
			});

			// Add admin listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				start_admin_service(state.clone(), ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving admin: {:?}", e))
			});

			// Wait for all servers to finish? I think this does what I want :shrug:
			while let Some(result) = run_set.join_next().await {
				#[allow(unused_must_use)]
				result.unwrap();
			}
		},
		Config::Xds(cfg) => {
			let ct = tokio_util::sync::CancellationToken::new();
			let metrics = xds::metrics::Metrics::new(&mut registry);
			let awaiting_ready = tokio::sync::watch::channel(()).0;
			let listener = inbound::Listener::from_xds(cfg.listener.clone())
				.await
				.unwrap();
			let state = Arc::new(tokio::sync::RwLock::new(ProxyState::new(listener.clone())));
			let state_clone = state.clone();
			let updater = ProxyStateUpdater::new(state_clone);
			let cfg_clone = cfg.clone();
			let xds_config = xds::client::Config::new(Arc::new(cfg_clone));
			let ads_client = xds_config
				.with_watched_handler::<XdsTarget>(xds::TARGET_TYPE, updater.clone())
				.with_watched_handler::<XdsRbac>(xds::RBAC_TYPE, updater)
				.build(metrics, awaiting_ready);

			let mut run_set = JoinSet::new();

			run_set.spawn(async move {
				ads_client
					.run()
					.await
					.map_err(|e| anyhow::anyhow!("error running xds client: {:?}", e))
			});

			// Add admin listener
			let ct_clone = ct.clone();
			let state_3 = state.clone();
			run_set.spawn(async move {
				start_admin_service(state_3, ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving admin: {:?}", e))
			});

			let relay_metrics = relay::metrics::Metrics::new(&mut registry);
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				listener
					.listen(state.clone(), Arc::new(relay_metrics), ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving static listener: {:?}", e))
			});

			// Add metrics listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				start_metrics_service(Arc::new(registry), ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving metrics: {:?}", e))
			});

			// Wait for all servers to finish? I think this does what I want :shrug:
			while let Some(result) = run_set.join_next().await {
				#[allow(unused_must_use)]
				result.unwrap();
			}
		},
	};

	Ok(())
}

async fn start_metrics_service(
	registry: Arc<Registry>,
	ct: tokio_util::sync::CancellationToken,
) -> Result<(), std::io::Error> {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:9091").await?;
	let app = MetricsApp::new(registry);
	let router = app.router();
	axum::serve(listener, router)
		.with_graceful_shutdown(async move {
			ct.cancelled().await;
		})
		.await
}

async fn start_admin_service(
	state: Arc<tokio::sync::RwLock<ProxyState>>,
	ct: tokio_util::sync::CancellationToken,
) -> Result<(), std::io::Error> {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:19000").await?;
	let app = AdminApp::new(state);
	let router = app.router();
	axum::serve(listener, router)
		.with_graceful_shutdown(async move {
			ct.cancelled().await;
		})
		.await
}
