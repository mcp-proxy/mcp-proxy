use crate::proto::mcpproxy::dev::common;
use crate::proto::mcpproxy::dev::listener::listener::authn;
use crate::proto::mcpproxy::dev::listener::listener;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use serde::Serialize;
use serde_json::Value;
use serde_json::map::Map;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum AuthError {
	InvalidToken(jsonwebtoken::errors::Error),
}

#[derive(Clone, Serialize)]
pub struct JwtAuthenticator {
	#[serde(skip_serializing)]
	key: Arc<RwLock<MutableKey>>,
	issuer: Option<HashSet<String>>,
	audience: Option<HashSet<String>>,

	remote: Option<JwksRemoteSource>,
}

impl std::fmt::Debug for JwtAuthenticator {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"JwtAuthenticator {{ issuer: {:?}, audience: {:?} }}",
			self.issuer, self.audience
		)
	}
}

#[derive(Debug)]
pub enum JwkError {
	JwksFetchError(reqwest::Error),
	JwksFileError(std::io::Error),
	JwksParseError(serde_json::Error),
	InvalidJWK(jsonwebtoken::errors::Error),
	UnsupportedAlgorithm,
	InvalidConfig(String),
}

#[derive(Clone, Serialize)]
struct JwksRemoteSource {
	#[serde(skip_serializing)]
	client: reqwest::Client,
	url: String,
	refresh_interval: Duration,
}

fn duration_from_pb(duration: Option<pbjson_types::Duration>, default: Duration) -> Duration {
	match duration {
		Some(duration) => {
			Duration::from_secs(duration.seconds as u64) + Duration::from_nanos(duration.nanos as u64)
		},
		None => default,
	}
}

impl JwksRemoteSource {
	fn from_xds(remote: &common::RemoteDataSource) -> Result<Self, JwkError> {
		let url = format!("{}:{}", remote.url, remote.port);
		let url = format!("{}/{}", url, remote.path);
		let client = reqwest::ClientBuilder::new()
			.timeout(duration_from_pb(
				remote.initial_timeout,
				Duration::from_secs(10),
			))
			.build()
			.map_err(JwkError::JwksFetchError)?;
		Ok(Self {
			client,
			url,
			refresh_interval: duration_from_pb(remote.refresh_interval, Duration::from_secs(10)),
		})
	}

	async fn fetch_jwks(&self) -> Result<Jwk, JwkError> {
		let response = self
			.client
			.get(&self.url)
			.send()
			.await
			.map_err(JwkError::JwksFetchError)?;
		let jwk: Jwk = serde_json::from_str(&response.text().await.map_err(JwkError::JwksFetchError)?)
			.map_err(JwkError::JwksParseError)?;
		Ok(jwk)
	}
}

impl JwtAuthenticator {
	pub async fn new(value: &authn::JwtConfig) -> Result<Self, JwkError> {
		let (jwk, remote): (Jwk, Option<JwksRemoteSource>) = match &value.jwks {
			Some(authn::jwt_config::Jwks::LocalJwks(local)) => match &local.source {
				Some(common::local_data_source::Source::Inline(jwk)) => {
					let jwk: Jwk = serde_json::from_slice(jwk).map_err(JwkError::JwksParseError)?;
					(jwk, None)
				},
				Some(common::local_data_source::Source::FilePath(path)) => {
					let file = std::fs::File::open(path).map_err(JwkError::JwksFileError)?;
					let jwk: Jwk = serde_json::from_reader(file).map_err(JwkError::JwksParseError)?;
					(jwk, None)
				},
				_ => {
					return Err(JwkError::InvalidConfig(
						"invalid local JWKS source".to_string(),
					));
				},
			},
			Some(authn::jwt_config::Jwks::RemoteJwks(remote)) => {
				let remote = JwksRemoteSource::from_xds(remote)?;
				let jwk = remote.fetch_jwks().await?;
				(jwk, Some(remote.clone()))
			},
			_ => {
				return Err(JwkError::InvalidConfig("no JWKS provided".to_string()));
			},
		};
		if !jwk.is_supported() {
			tracing::error!("unsupported algorithm");
			return Err(JwkError::UnsupportedAlgorithm);
		}
		let issuer = match value.issuer.len() {
			0 => None,
			_ => Some(HashSet::<String>::from_iter(
				value.issuer.iter().map(|s| s.to_string()),
			)),
		};
		let audience = match value.audience.len() {
			0 => None,
			_ => Some(HashSet::<String>::from_iter(
				value.audience.iter().map(|s| s.to_string()),
			)),
		};
		Ok(JwtAuthenticator {
			key: Arc::new(RwLock::new(
				DecodingKey::from_jwk(&jwk)
					.map_err(JwkError::InvalidJWK)?
					.into(),
			)),
			issuer,
			audience,
			remote,
		})
	}

	pub async fn sync_jwks(&mut self) -> Result<(), JwkError> {
		match &self.remote {
			Some(remote) => {
				let jwk = remote.fetch_jwks().await?;
				self
					.key
					.write()
					.await
					.update(DecodingKey::from_jwk(&jwk).map_err(JwkError::InvalidJWK)?);
				Ok(())
			},
			None => Ok(()),
		}
	}
}

pub async fn sync_jwks_loop(
	authn: Arc<RwLock<Option<JwtAuthenticator>>>,
	ct: CancellationToken,
) -> Result<(), JwkError> {
	let interval: Duration = authn
		.read()
		.await
		.as_ref()
		.map_or(Duration::from_secs(10), |authn| {
			authn
				.remote
				.as_ref()
				.map_or(Duration::from_secs(10), |remote| remote.refresh_interval)
		});
	loop {
		tokio::select! {
			_ = ct.cancelled() => {
				tracing::info!("cancelled sync_jwks_loop");
				return Ok(());
			},
			_ = tokio::time::sleep(interval) => {
				let mut authenticator = authn.write().await;
				match authenticator.as_mut() {
					Some(authenticator) => match authenticator.sync_jwks().await {
						Ok(_) => {
							tracing::trace!("synced jwks");
						},
						Err(e) => {
							tracing::error!("error syncing jwks: {:?}", e);
						},
					},
					None => {
						tracing::trace!("no authenticator, skipping sync");
					},
				}
				drop(authenticator);
			}
		}
	}
}

// MutableKey is a wrapper around DecodingKey that allows us to update the key atomically
pub struct MutableKey {
	key: DecodingKey,
}

impl MutableKey {
	pub fn new(key: DecodingKey) -> Self {
		Self { key }
	}

	pub fn update(&mut self, key: DecodingKey) {
		self.key = key;
	}
}

impl From<DecodingKey> for MutableKey {
	fn from(key: DecodingKey) -> Self {
		Self::new(key)
	}
}

impl JwtAuthenticator {
	pub async fn authenticate(&self, token: &str) -> Result<crate::rbac::Claims, AuthError> {
		let header = decode_header(token).map_err(AuthError::InvalidToken)?;

		let validation = {
			let mut validation = Validation::new(header.alg);
			validation.aud = self.audience.clone();
			validation.iss = self.issuer.clone();
			validation
		};

		let key = self.key.read().await;
		let token_data = decode::<Map<String, Value>>(token, &key.key, &validation)
			.map_err(AuthError::InvalidToken)?;
		tracing::info!("token data: {:?}", token_data);
		Ok(crate::rbac::Claims::new(token_data.claims))
	}
}
