use crate::backend::BackendAuth;
use crate::inbound::Listener;
use crate::metrics::Recorder;
use crate::outbound::openapi;
use crate::outbound::{Target, TargetSpec};
use crate::rbac;
use crate::rbac::Identity;
use crate::relay::metrics;
use crate::xds::XdsStore;
use a2a_sdk::AgentCard;
use anyhow::{Context, anyhow};
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::TryStreamExt;
use http::HeaderName;
use http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use itertools::Itertools;
use reqwest::Body;
use rmcp::RoleClient;
use rmcp::service::RunningService;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::sse::{ReqwestSseClient, SseTransport};
use rmcp::{
	Error as McpError, RoleServer, ServerHandler, model::CallToolRequestParam, model::Tool, model::*,
	service::RequestContext,
};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tracing::instrument;

lazy_static::lazy_static! {
	static ref DEFAULT_ID: rbac::Identity = rbac::Identity::default();
}

#[derive(Clone)]
pub struct Relay {
	state: Arc<tokio::sync::RwLock<XdsStore>>,
	pool: Arc<RwLock<pool::ConnectionPool>>,
	metrics: Arc<metrics::Metrics>,
}

impl Relay {
	pub fn new(state: Arc<tokio::sync::RwLock<XdsStore>>, metrics: Arc<metrics::Metrics>) -> Self {
		Self {
			state: state.clone(),
			pool: Arc::new(RwLock::new(pool::ConnectionPool::new(state.clone()))),
			metrics,
		}
	}
}

impl Relay {
	pub async fn remove_target(&self, name: &str) -> Result<(), tokio::task::JoinError> {
		tracing::info!("removing target: {}", name);
		let mut pool = self.pool.write().await;
		match pool.remove(name).await {
			Some(target_arc) => {
				// Try this a few times?
				let target = Arc::into_inner(target_arc).unwrap();
				match target {
					UpstreamTarget::Mcp(m) => {
						m.cancel().await?;
					},
					_ => {
						todo!()
					},
				}
				Ok(())
			},
			None => Ok(()),
		}
	}
}

pub enum Response {
	Streaming(ReceiverStream<a2a_sdk::ClientJsonRpcMessage>),
	Single(a2a_sdk::ClientJsonRpcMessage)
}

impl Relay {
	pub async fn fetch_agent_card(
		&self,
		host: String,
		claims: Identity,
		service_name: &str,
	) -> anyhow::Result<AgentCard> {
		let mut pool = self.pool.write().await;
		let svc = pool
			.get_or_create(service_name)
			.await
			.context(format!("Service {} not found", service_name))?;
		let mut card = svc.fetch_agent_card().await?;
		let url = {
			let state = self.state.read().await;
			match &state.listener {
				Listener::A2a(a) => a.url(host),
				_ => {
					panic!("must be a2a")
				},
			}
		};
		card.url = format!("{}/{}", url, service_name);

		let state = self.state.read().await;
		let pols = &state.policies;
		card.skills = card
			.skills
			.iter()
			.filter(|s| {
				// TODO for now we treat it as a 'tool'
				let tool_name = format!("{}:{}", service_name, s.name);
				pols.validate(&rbac::ResourceType::Tool { id: tool_name }, &claims)
			})
			.cloned()
			.collect();
		Ok(card)
	}
	pub async fn proxy(
		self,
		request: a2a_sdk::A2aRequest,
		claims: Identity,
		service_name: String,
	) -> anyhow::Result<Response> {
		use futures::StreamExt;
		let mut pool = self.pool.write().await;
		let svc = pool
			.get_or_create(&service_name)
			.await
			.context(format!("Service {} not found", &service_name))?;
		let client = svc.fetch_client()?;
		let (to_client_tx, to_client_rx) =
			tokio::sync::mpsc::channel::<a2a_sdk::ClientJsonRpcMessage>(64);
		let resp = client.json(&request).send().await?;

		let content = resp
			.headers()
			.get(reqwest::header::CONTENT_TYPE)
			.and_then(|value| value.to_str().ok())
			.and_then(|value| value.parse::<mime::Mime>().ok())
			.map(|mime| mime.type_().as_str().to_string() + "/" + mime.subtype().as_str());
		tracing::error!("howardjohn: got response code={} content_type={:?}", resp.status(), content);
		match content.as_deref() {
			Some(("application/json")) => {
				let j = resp
					.json::<a2a_sdk::ClientJsonRpcMessage>()
					.await
					.expect("TODO handle error");
				Ok(Response::Single(j))
			},
			Some(("text/event-stream")) => {
				tokio::spawn(async move {
					let mut events = resp.bytes_stream().eventsource();

					while let Some(thing) = events.next().await {
						let event = thing.expect("TODO");
						tracing::error!("howardjohn: got event {:?}", event);
						if event.event == "message" {
							tracing::error!("howardjohn: data: {}", event.data.to_string());
							let j: a2a_sdk::ClientJsonRpcMessage =
								serde_json::from_str(&event.data).expect("TODO handle error");
							tracing::error!("howardjohn: sent {j:?}");
							to_client_tx.send(j).await.unwrap();
						}
					}
					drop(to_client_tx);
				});

				Ok(Response::Streaming(ReceiverStream::new(to_client_rx)))
			},
			_ => anyhow::bail!("expected JSON or event stream"),
		}
	}
	#[instrument(level = "debug", skip_all)]
	fn get_info(&self) -> ServerInfo {
		ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities {
                completions: None,
                experimental: None,
                logging: None,
                prompts: Some(PromptsCapability::default()),
                resources: Some(ResourcesCapability::default()),
                tools: Some(ToolsCapability {
                    list_changed: None,
                }),
            },
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "This server provides a counter tool that can increment and decrement values. The counter starts at 0 and can be modified using the 'increment' and 'decrement' tools. Use 'get_value' to check the current count.".to_string(),
            ),
        }
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_resources(
		&self,
		request: Option<PaginatedRequestParam>,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourcesResult, McpError> {
		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {}", e), None))?;
		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_resources(request).await {
					Ok(r) => Ok(r.resources),
					Err(e) => Err(e),
				}
			}
		});

		// TODO: Handle errors
		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		Ok(ListResourcesResult {
			resources: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_resource_templates(
		&self,
		request: Option<PaginatedRequestParam>,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListResourceTemplatesResult, McpError> {
		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {}", e), None))?;
		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_resource_templates(request).await {
					Ok(r) => Ok(r.resource_templates),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.clone().record(
			&metrics::ListCall {
				resource_type: "resource_template".to_string(),
			},
			(),
		);

		Ok(ListResourceTemplatesResult {
			resource_templates: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_prompts(
		&self,
		request: Option<PaginatedRequestParam>,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListPromptsResult, McpError> {
		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {}", e), None))?;
		let all = connections.into_iter().map(|(_name, svc)| {
			let request = request.clone();
			async move {
				match svc.list_prompts(request).await {
					Ok(r) => Ok(
						r.prompts
							.into_iter()
							.map(|p| Prompt {
								name: format!("{}:{}", _name, p.name),
								description: p.description,
								arguments: p.arguments,
							})
							.collect::<Vec<_>>(),
					),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.clone().record(
			&metrics::ListCall {
				resource_type: "prompt".to_string(),
			},
			(),
		);
		Ok(ListPromptsResult {
			prompts: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.uri,
        ),
    )]
	async fn read_resource(
		&self,
		request: ReadResourceRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ReadResourceResult, McpError> {
		if !self.state.read().await.policies.validate(
			&rbac::ResourceType::Resource {
				id: request.uri.to_string(),
			},
			match _context.extensions.get::<rbac::Identity>() {
				Some(id) => id,
				None => &DEFAULT_ID,
			},
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}

		let uri = request.uri.to_string();
		let (service_name, resource) = uri.split_once(':').unwrap();
		let mut pool = self.pool.write().await;
		let service_arc = pool.get_or_create(service_name).await.map_err(|_e| {
			McpError::invalid_request(format!("Service {} not found", service_name), None)
		})?;
		let req = ReadResourceRequestParam {
			uri: resource.to_string(),
		};

		self.metrics.clone().record(
			&metrics::GetResourceCall {
				server: service_name.to_string(),
				uri: resource.to_string(),
			},
			(),
		);
		match service_arc.read_resource(req).await {
			Ok(r) => Ok(r),
			Err(e) => Err(e.into()),
		}
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.name,
        ),
    )]
	async fn get_prompt(
		&self,
		request: GetPromptRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<GetPromptResult, McpError> {
		if !self.state.read().await.policies.validate(
			&rbac::ResourceType::Prompt {
				id: request.name.to_string(),
			},
			match _context.extensions.get::<rbac::Identity>() {
				Some(id) => id,
				None => &DEFAULT_ID,
			},
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}

		let prompt_name = request.name.to_string();
		let (service_name, prompt) = prompt_name.split_once(':').unwrap();
		let mut pool = self.pool.write().await;
		let svc = pool.get_or_create(service_name).await.map_err(|_e| {
			McpError::invalid_request(format!("Service {} not found", service_name), None)
		})?;
		let req = GetPromptRequestParam {
			name: prompt.to_string(),
			arguments: request.arguments,
		};

		self.metrics.clone().record(
			&metrics::GetPromptCall {
				server: service_name.to_string(),
				name: prompt.to_string(),
			},
			(),
		);
		match svc.get_prompt(req).await {
			Ok(r) => Ok(r),
			Err(e) => Err(e.into()),
		}
	}

	#[instrument(level = "debug", skip_all)]
	async fn list_tools(
		&self,
		request: Option<PaginatedRequestParam>,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<ListToolsResult, McpError> {
		// TODO: Use iterators
		// TODO: Handle individual errors
		// TODO: Do we want to handle pagination here, or just pass it through?
		let mut pool = self.pool.write().await;
		let connections = pool
			.list()
			.await
			.map_err(|e| McpError::internal_error(format!("Failed to list connections: {}", e), None))?;
		let all = connections.into_iter().map(|(_name, svc_arc)| {
			let request = request.clone();
			async move {
				match svc_arc.list_tools(request).await {
					Ok(r) => Ok(
						r.tools
							.into_iter()
							.map(|t| Tool {
								annotations: None,
								name: Cow::Owned(format!("{}:{}", _name, t.name)),
								description: t.description,
								input_schema: t.input_schema,
							})
							.collect::<Vec<_>>(),
					),
					Err(e) => Err(e),
				}
			}
		});

		let (results, _errors): (Vec<_>, Vec<_>) = futures::future::join_all(all)
			.await
			.into_iter()
			.partition_result();

		self.metrics.clone().record(
			&metrics::ListCall {
				resource_type: "tool".to_string(),
			},
			(),
		);

		Ok(ListToolsResult {
			tools: results.into_iter().flatten().collect(),
			next_cursor: None,
		})
	}

	#[instrument(
        level = "debug",
        skip_all,
        fields(
        name=%request.name,
        ),
    )]
	async fn call_tool(
		&self,
		request: CallToolRequestParam,
		_context: RequestContext<RoleServer>,
	) -> std::result::Result<CallToolResult, McpError> {
		if !self.state.read().await.policies.validate(
			&rbac::ResourceType::Tool {
				id: request.name.to_string(),
			},
			match _context.extensions.get::<rbac::Identity>() {
				Some(id) => id,
				None => &DEFAULT_ID,
			},
		) {
			return Err(McpError::invalid_request("not allowed", None));
		}
		let tool_name = request.name.to_string();
		let (service_name, tool) = tool_name
			.split_once(':')
			.ok_or(McpError::invalid_request("invalid tool name", None))?;
		let mut pool = self.pool.write().await;
		let svc = pool.get_or_create(service_name).await.map_err(|_e| {
			McpError::invalid_request(format!("Service {} not found", service_name), None)
		})?;
		let req = CallToolRequestParam {
			name: Cow::Owned(tool.to_string()),
			arguments: request.arguments,
		};

		self.metrics.clone().record(
			&metrics::ToolCall {
				server: service_name.to_string(),
				name: tool.to_string(),
			},
			(),
		);

		match svc.call_tool(req).await {
			Ok(r) => Ok(r),
			Err(e) => {
				self.metrics.clone().record(
					&metrics::ToolCallError {
						server: service_name.to_string(),
						name: tool.to_string(),
						error_type: e.error_code(),
					},
					(),
				);
				Err(e.into())
			},
		}
	}
}

mod pool {
	use rmcp::service::serve_client_with_ct;

	use super::*;

	pub(crate) struct ConnectionPool {
		state: Arc<tokio::sync::RwLock<XdsStore>>,
		by_name: HashMap<String, Arc<UpstreamTarget>>,
	}

	impl ConnectionPool {
		pub(crate) fn new(state: Arc<tokio::sync::RwLock<XdsStore>>) -> Self {
			Self {
				state,
				by_name: HashMap::new(),
			}
		}

		pub(crate) async fn get_or_create(
			&mut self,
			name: &str,
		) -> anyhow::Result<Arc<UpstreamTarget>> {
			// Connect if it doesn't exist
			if !self.by_name.contains_key(name) {
				// Read target info and drop lock before calling connect
				let target_info: Option<(Target, tokio_util::sync::CancellationToken)> = {
					let state = self.state.read().await;
					state
						.targets
						.get(name)
						.map(|(target, ct)| (target.clone(), ct.clone()))
				};

				if let Some((target, ct)) = target_info {
					// Now self is not immutably borrowed by state lock
					self.connect(&ct, &target).await?;
				} else {
					// Handle target not found in state configuration
					return Err(anyhow::anyhow!(
						"Target configuration not found for {}",
						name
					));
				}
			}
			let target = self.by_name.get(name).cloned();
			Ok(target.ok_or(McpError::invalid_request(
				format!("Service {} not found", name),
				None,
			))?)
		}

		pub(crate) async fn remove(&mut self, name: &str) -> Option<Arc<UpstreamTarget>> {
			self.by_name.remove(name)
		}

		pub(crate) async fn list(&mut self) -> anyhow::Result<Vec<(String, Arc<UpstreamTarget>)>> {
			// Iterate through all state targets, and get the connection from the pool
			// If the connection is not in the pool, connect to it and add it to the pool
			// 1. Get target configurations (name, Target, CancellationToken) from the state's TargetStore
			let targets_config: Vec<(String, (Target, tokio_util::sync::CancellationToken))> = {
				let state = self.state.read().await;
				// Iterate the underlying HashMap directly to get the full tuple
				state
					.targets
					.iter()
					.map(|(name, target)| (name.clone(), target.clone()))
					.collect()
			};

			// 2. Identify targets needing connection without holding lock or borrowing self mutably yet
			let mut connections_to_make = Vec::new();
			for (name, (target, ct)) in &targets_config {
				if !self.by_name.contains_key(name) {
					connections_to_make.push((name.clone(), target.clone(), ct.clone()));
				}
			}

			// 3. Connect the missing ones (self is borrowed mutably here)
			for (name, target, ct) in connections_to_make {
				tracing::debug!("Connecting missing target: {}", name);
				self.connect(&ct, &target).await.map_err(|e| {
					tracing::error!("Failed to connect target {}: {}", name, e);
					e // Propagate error
				})?;
			}
			tracing::debug!("Finished connecting missing targets.");

			// 4. Collect all required connections from the pool
			let results = targets_config
				.into_iter()
				.filter_map(|(name, _)| self.by_name.get(&name).map(|arc| (name, arc.clone())))
				.collect();

			Ok(results)
		}

		#[instrument(
            level = "debug",
            skip_all,
            fields(
          name=%target.name,
            ),
        )]
		pub(crate) async fn connect(
			&mut self,
			ct: &tokio_util::sync::CancellationToken,
			target: &Target,
		) -> Result<(), anyhow::Error> {
			// Already connected
			if let Some(_transport) = self.by_name.get(&target.name) {
				return Ok(());
			}
			tracing::trace!("connecting to target: {}", target.name);
			let transport: UpstreamTarget = match &target.spec {
				TargetSpec::Sse {
					host,
					port,
					path,
					backend_auth,
					headers,
				} => {
					tracing::trace!("starting sse transport for target: {}", target.name);
					let path = match path.as_str() {
						"" => "/sse",
						_ => path,
					};
					let scheme = match port {
						443 => "https",
						_ => "https",
					};

					let url = format!("{}://{}:{}{}", scheme, host, port, path);
					let transport = match backend_auth.clone() {
						Some(backend_auth) => {
							let backend_auth = backend_auth.build().await;
							let token = backend_auth.get_token().await?;
							let mut upstream_headers = HeaderMap::new();
							let auth_value = HeaderValue::from_str(token.as_str())?;
							upstream_headers.insert(AUTHORIZATION, auth_value);
							for (key, value) in headers {
								upstream_headers.insert(
									HeaderName::from_bytes(key.as_bytes())?,
									HeaderValue::from_str(value)?,
								);
							}
							let client = reqwest::Client::builder()
								.default_headers(upstream_headers)
								.build()
								.unwrap();
							let client = ReqwestSseClient::new_with_client(url.as_str(), client).await?;
							SseTransport::start_with_client(client).await?
						},
						None => {
							let client = ReqwestSseClient::new(url.as_str())?;
							SseTransport::start_with_client(client).await?
						},
					};

					UpstreamTarget::Mcp(serve_client_with_ct((), transport, ct.child_token()).await?)
				},
				TargetSpec::A2a {
					host,
					port,
					path,
					backend_auth,
					headers,
				} => {
					tracing::info!("starting A2a transport for target: {}", target.name);
					let client = reqwest::Client::new();

					let scheme = match port {
						443 => "https",
						_ => "http",
					};
					let url = format!("{}://{}:{}{}", scheme, host, port, path);

					let client = match backend_auth.clone() {
						Some(backend_auth) => {
							let backend_auth = backend_auth.build().await;
							let token = backend_auth.get_token().await?;
							let mut upstream_headers = HeaderMap::new();
							let auth_value = HeaderValue::from_str(token.as_str())?;
							upstream_headers.insert(AUTHORIZATION, auth_value);
							for (key, value) in headers {
								upstream_headers.insert(
									HeaderName::from_bytes(key.as_bytes())?,
									HeaderValue::from_str(value)?,
								);
							}
							reqwest::Client::builder()
								.default_headers(upstream_headers)
								.build()
								.unwrap()
						},
						None => reqwest::Client::new(),
					};
					UpstreamTarget::A2a(crate::a2a::Client {
						url: reqwest::Url::parse(&url).expect("failed to parse url"),
						client,
					})
				},
				TargetSpec::Stdio { cmd, args, env: _ } => {
					tracing::trace!("starting stdio transport for target: {}", target.name);
					UpstreamTarget::Mcp(
						serve_client_with_ct(
							(),
							TokioChildProcess::new(Command::new(cmd).args(args)).unwrap(),
							ct.child_token(),
						)
						.await?,
					)
				},
				TargetSpec::OpenAPI(open_api) => {
					tracing::info!("starting OpenAPI transport for target: {}", target.name);
					let client = reqwest::Client::new();

					let scheme = match open_api.port {
						443 => "https",
						_ => "http",
					};
					UpstreamTarget::OpenAPI(openapi::Handler {
						host: open_api.host.clone(),
						client,
						tools: open_api.tools.clone(),
						scheme: scheme.to_string(),
						prefix: open_api.prefix.clone(),
						port: open_api.port,
					})
				},
			};
			self
				.by_name
				.insert(target.name.clone(), Arc::new(transport));
			Ok(())
		}
	}
}

// UpstreamTarget defines a source for MCP information.
#[derive(Debug)]
enum UpstreamTarget {
	Mcp(RunningService<RoleClient, ()>),
	OpenAPI(openapi::Handler),
	A2a(crate::a2a::Client),
}

enum UpstreamError {
	ServiceError(rmcp::ServiceError),
	OpenAPIError(anyhow::Error),
	Unsupported,
}

impl UpstreamError {
	fn error_code(&self) -> String {
		match self {
			Self::ServiceError(e) => match e {
				rmcp::ServiceError::McpError(_) => "mcp_error".to_string(),
				rmcp::ServiceError::Timeout { timeout: _ } => "timeout".to_string(),
				rmcp::ServiceError::Cancelled { reason } => {
					reason.clone().unwrap_or("cancelled".to_string())
				},
				rmcp::ServiceError::UnexpectedResponse => "unexpected_response".to_string(),
				rmcp::ServiceError::Transport(_) => "transport_error".to_string(),
				_ => "unknown".to_string(),
			},
			Self::OpenAPIError(_) => "openapi_error".to_string(),
			Self::Unsupported => "unsupported operation".to_string(),
		}
	}
}
impl From<rmcp::ServiceError> for UpstreamError {
	fn from(value: rmcp::ServiceError) -> Self {
		UpstreamError::ServiceError(value)
	}
}

impl From<anyhow::Error> for UpstreamError {
	fn from(value: anyhow::Error) -> Self {
		UpstreamError::OpenAPIError(value)
	}
}

impl From<UpstreamError> for ErrorData {
	fn from(value: UpstreamError) -> Self {
		match value {
			UpstreamError::OpenAPIError(e) => ErrorData::internal_error(e.to_string(), None),
			UpstreamError::ServiceError(e) => match e {
				rmcp::ServiceError::McpError(e) => e,
				rmcp::ServiceError::Timeout { timeout } => {
					ErrorData::internal_error(format!("request timed out after {:?}", timeout), None)
				},
				rmcp::ServiceError::Cancelled { reason } => match reason {
					Some(reason) => ErrorData::internal_error(reason.clone(), None),
					None => ErrorData::internal_error("unknown reason", None),
				},
				rmcp::ServiceError::UnexpectedResponse => {
					ErrorData::internal_error("unexpected response", None)
				},
				rmcp::ServiceError::Transport(e) => ErrorData::internal_error(e.to_string(), None),
				_ => ErrorData::internal_error("unknown error", None),
			},
			UpstreamError::Unsupported => ErrorData::internal_error("unsupported operation", None),
		}
	}
}

impl UpstreamTarget {
	async fn list_tools(
		&self,
		request: Option<PaginatedRequestParam>,
	) -> Result<ListToolsResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.list_tools(request).await?),
			UpstreamTarget::OpenAPI(m) => Ok(ListToolsResult {
				next_cursor: None,
				tools: m.tools(),
			}),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn get_prompt(
		&self,
		request: GetPromptRequestParam,
	) -> Result<GetPromptResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.get_prompt(request).await?),
			UpstreamTarget::OpenAPI(_) => Ok(GetPromptResult {
				description: None,
				messages: vec![],
			}),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn list_prompts(
		&self,
		request: Option<PaginatedRequestParam>,
	) -> Result<ListPromptsResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.list_prompts(request).await?),
			UpstreamTarget::OpenAPI(_) => Ok(ListPromptsResult {
				next_cursor: None,
				prompts: vec![],
			}),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn list_resources(
		&self,
		request: Option<PaginatedRequestParam>,
	) -> Result<ListResourcesResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.list_resources(request).await?),
			UpstreamTarget::OpenAPI(_) => Ok(ListResourcesResult {
				next_cursor: None,
				resources: vec![],
			}),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn list_resource_templates(
		&self,
		request: Option<PaginatedRequestParam>,
	) -> Result<ListResourceTemplatesResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.list_resource_templates(request).await?),
			UpstreamTarget::OpenAPI(_) => Ok(ListResourceTemplatesResult {
				next_cursor: None,
				resource_templates: vec![],
			}),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn read_resource(
		&self,
		request: ReadResourceRequestParam,
	) -> Result<ReadResourceResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.read_resource(request).await?),
			UpstreamTarget::OpenAPI(_) => Ok(ReadResourceResult { contents: vec![] }),
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn call_tool(
		&self,
		request: CallToolRequestParam,
	) -> Result<CallToolResult, UpstreamError> {
		match self {
			UpstreamTarget::Mcp(m) => Ok(m.call_tool(request).await?),
			UpstreamTarget::OpenAPI(m) => {
				let res = m
					.call_tool(request.name.as_ref(), request.arguments)
					.await?;
				Ok(CallToolResult {
					content: vec![Content::text(res)],
					is_error: None,
				})
			},
			UpstreamTarget::A2a(_) => Err(UpstreamError::Unsupported),
		}
	}

	async fn fetch_agent_card(&self) -> Result<AgentCard, anyhow::Error> {
		match self {
			UpstreamTarget::Mcp(m) => Err(anyhow!("unsupported")),
			UpstreamTarget::OpenAPI(m) => Err(anyhow!("unsupported")),
			UpstreamTarget::A2a(m) => Ok(
				m.client
					.get(format!("{}.well-known/agent.json", m.url))
					.header("Content-type", "application/json")
					.send()
					.await?
					.json()
					.await?,
			),
		}
	}
	fn fetch_client(&self) -> Result<reqwest::RequestBuilder, anyhow::Error> {
		match self {
			UpstreamTarget::Mcp(m) => Err(anyhow!("unsupported")),
			UpstreamTarget::OpenAPI(m) => Err(anyhow!("unsupported")),
			UpstreamTarget::A2a(m) => Ok(m.client.post(m.url.clone())),
		}
	}
}

struct SerializeStream<T>(T);

impl<T: Serialize> Into<bytes::Bytes> for SerializeStream<T> {
	fn into(self) -> Bytes {
		Bytes::from(serde_json::to_vec(&self.0).unwrap())
	}
}
