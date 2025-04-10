use crate::proto;
use crate::proto::mcpproxy::dev::target::target::OpenApiTarget as XdsOpenAPITarget;
use openapiv3::OpenAPI;
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
pub mod backend;
pub mod openapi;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Target {
	pub name: String,
	pub spec: TargetSpec,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum TargetSpec {
	Sse {
		host: String,
		port: u32,
		path: String,
		headers: HashMap<String, String>,
		backend_auth: Option<backend::BackendAuthConfig>,
	},
	A2a {
		host: String,
		port: u32,
		path: String,
		headers: HashMap<String, String>,
		backend_auth: Option<backend::BackendAuthConfig>,
	},
	Stdio {
		cmd: String,
		args: Vec<String>,
		env: HashMap<String, String>,
	},
	OpenAPI(OpenAPITarget),
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct OpenAPITarget {
	pub host: String,
	pub prefix: String,
	pub port: u16,
	pub tools: Vec<(Tool, openapi::UpstreamOpenAPICall)>,
}

impl TryFrom<XdsOpenAPITarget> for OpenAPITarget {
	type Error = openapi::ParseError;

	fn try_from(value: XdsOpenAPITarget) -> Result<Self, Self::Error> {
		let schema = value.schema.ok_or(openapi::ParseError::MissingSchema)?;
		let schema_bytes =
			proto::resolve_local_data_source(&schema.source.ok_or(openapi::ParseError::MissingFields)?)?;
		let schema: OpenAPI =
			serde_json::from_slice(&schema_bytes).map_err(openapi::ParseError::SerdeError)?;
		let tools = openapi::parse_openapi_schema(&schema)?;
		let prefix = openapi::get_server_prefix(&schema)?;
		Ok(OpenAPITarget {
			host: value.host.clone(),
			prefix,
			port: value.port as u16, // TODO: check if this is correct
			tools,
		})
	}
}
