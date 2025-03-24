// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::error::Error as StdErr;
use std::fmt;
use std::fmt::Formatter;
use std::sync::{Arc, RwLock};
use tracing::Level;

use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, trace, warn};

pub use client::*;
pub use metrics::*;
pub use types::*;

use xds::mcp::kgateway_dev::rbac::Config as XdsRbac;
use xds::mcp::kgateway_dev::target::Target as XdsTarget;

use self::envoy::service::discovery::v3::DeltaDiscoveryRequest;
use crate::rbac;
use crate::strng::Strng;
use crate::xds;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub mod client;
pub mod metrics;
pub mod types;

struct DisplayStatus<'a>(&'a tonic::Status);

impl fmt::Display for DisplayStatus<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		let s = &self.0;
		write!(f, "status: {:?}, message: {:?}", s.code(), s.message())?;

		if s.message().to_string().contains("authentication failure") {
			write!(
				f,
				" (hint: check the control plane logs for more information)"
			)?;
		}
		if !s.details().is_empty() {
			if let Ok(st) = std::str::from_utf8(s.details()) {
				write!(f, ", details: {st}")?;
			}
		}
		if let Some(src) = s.source().and_then(|s| s.source()) {
			write!(f, ", source: {src}")?;
			// Error is not public to explicitly match on, so do a fuzzy match
			if format!("{src}").contains("Temporary failure in name resolution") {
				write!(f, " (hint: is the DNS server reachable?)")?;
			}
		}
		Ok(())
	}
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("gRPC error {}", DisplayStatus(.0))]
	GrpcStatus(#[from] tonic::Status),
	#[error("gRPC connection error connecting to {}: {}", .0, DisplayStatus(.1))]
	Connection(String, #[source] tonic::Status),
	/// Attempted to send on a MPSC channel which has been canceled
	#[error(transparent)]
	RequestFailure(#[from] Box<mpsc::error::SendError<DeltaDiscoveryRequest>>),
	#[error("transport error: {0}")]
	Transport(#[from] tonic::transport::Error),
	// #[error("failed to send on demand resource")]
	// OnDemandSend(),
	// #[error("TLS Error: {0}")]
	// TLSError(#[from] tls::Error),
}

/// Updates the [ProxyState] from XDS.
/// All state updates code goes in ProxyStateUpdateMutator, that takes state as a parameter.
/// this guarantees that the state is always locked when it is updated.
#[derive(Clone)]
pub struct ProxyStateUpdateMutator {}

#[derive(Clone)]
pub struct ProxyStateUpdater {
	state: Arc<RwLock<XdsStore>>,
	updater: ProxyStateUpdateMutator,
}

impl ProxyStateUpdater {
	/// Creates a new updater for the given stores. Will prefetch certs when workloads are updated.
	pub fn new(state: Arc<RwLock<XdsStore>>) -> Self {
		Self {
			state,
			updater: ProxyStateUpdateMutator {},
		}
	}
}

impl ProxyStateUpdateMutator {
	#[instrument(
        level = Level::TRACE,
        name="insert_target",
        skip_all,
        fields(name=%target.name),
    )]
	pub fn insert_target(&self, state: &mut XdsStore, target: XdsTarget) -> anyhow::Result<()> {
		let target = Target::from(&target);
		// TODO: This is a hack
		// TODO: Separate connection/LB from insertion
		state.targets.insert(target);
		Ok(())
	}

	#[instrument(
        level = Level::TRACE,
        name="remove_target",
        skip_all,
        fields(name=%xds_name),
    )]
	pub fn remove_target(&self, state: &mut XdsStore, xds_name: &Strng) {
		state.targets.remove(xds_name);
	}

	#[instrument(
        level = Level::TRACE,
        name="insert_rbac",
        skip_all,
    )]
	pub fn insert_rbac(&self, state: &mut XdsStore, rbac: XdsRbac) -> anyhow::Result<()> {
		let rule_set = rbac::RuleSet::from(&rbac);
		state.policies.insert(rule_set);
		Ok(())
	}

	#[instrument(
        level = Level::TRACE,
        name="remove_rbac",
        skip_all,
        fields(name=%xds_name),
    )]
	pub fn remove_rbac(&self, state: &mut XdsStore, xds_name: &Strng) {
		state.policies.remove(xds_name);
	}
}

impl Handler<XdsTarget> for ProxyStateUpdater {
	fn handle(
		&self,
		updates: Box<&mut dyn Iterator<Item = XdsUpdate<XdsTarget>>>,
	) -> Result<(), Vec<RejectedConfig>> {
		let mut state = self.state.write().unwrap();
		let handle = |res: XdsUpdate<XdsTarget>| {
			match res {
				XdsUpdate::Update(w) => self.updater.insert_target(&mut state, w.resource)?,
				XdsUpdate::Remove(name) => self.updater.remove_target(&mut state, &name),
			}
			Ok(())
		};
		handle_single_resource(updates, handle)
	}
}

impl Handler<XdsRbac> for ProxyStateUpdater {
	fn handle(
		&self,
		updates: Box<&mut dyn Iterator<Item = XdsUpdate<XdsRbac>>>,
	) -> Result<(), Vec<RejectedConfig>> {
		let mut state = self.state.write().unwrap();
		let handle = |res: XdsUpdate<XdsRbac>| {
			match res {
				XdsUpdate::Update(w) => self.updater.insert_rbac(&mut state, w.resource)?,
				XdsUpdate::Remove(name) => self.updater.remove_rbac(&mut state, &name),
			}
			Ok(())
		};
		handle_single_resource(updates, handle)
	}
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct Target {
	pub name: String,
	pub spec: TargetSpec,
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type")]
pub enum TargetSpec {
	#[serde(rename = "sse")]
	Sse { host: String, port: u32 },
	#[serde(rename = "stdio")]
	Stdio { cmd: String, args: Vec<String> },
}

impl From<&XdsTarget> for Target {
	fn from(value: &XdsTarget) -> Self {
		Target {
			name: value.name.clone(),
			spec: {
				TargetSpec::Sse {
					host: value.host.clone(),
					port: value.port,
				}
			},
		}
	}
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type")]
pub enum Listener {
	#[serde(rename = "sse")]
	Sse {
		host: String,
		port: u32,
		mode: Option<ListenerMode>,
	},
	#[serde(rename = "stdio")]
	Stdio {},
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub enum ListenerMode {
	#[serde(rename = "proxy")]
	Proxy,
}

impl Default for Listener {
	fn default() -> Self {
		Self::Stdio {}
	}
}

pub struct TargetStore {
	by_name: HashMap<String, Target>,
}

impl TargetStore {
	pub fn new() -> Self {
		Self {
			by_name: HashMap::new(),
		}
	}

	pub fn remove(&mut self, name: &str) {
		// TODO: Drain connections from target
		self.by_name.remove(name);
	}

	pub fn insert(&mut self, target: Target) {
		self.by_name.insert(target.name.clone(), target);
	}

	pub fn get(&self, name: &str) -> Option<&Target> {
		self.by_name.get(name)
	}

	pub fn iter(&self) -> impl Iterator<Item = (String, &Target)> {
		self
			.by_name
			.iter()
			.map(|(name, target)| (name.clone(), target))
	}

	pub fn clear(&mut self) {
		self.by_name.clear();
	}
}

pub struct PolicyStore {
	by_name: HashMap<String, rbac::RuleSet>,
}

impl PolicyStore {
	pub fn new() -> Self {
		Self {
			by_name: HashMap::new(),
		}
	}
}

impl PolicyStore {
	pub fn insert(&mut self, policy: rbac::RuleSet) {
		self.by_name.insert(policy.to_key(), policy);
	}

	pub fn remove(&mut self, name: &str) {
		self.by_name.remove(name);
	}

	pub fn validate(&self, resource: &rbac::ResourceType, claims: &rbac::Identity) -> bool {
		self
			.by_name
			.values()
			.into_iter()
			.any(|policy| policy.validate(resource, claims))
	}

	pub fn clear(&mut self) {
		self.by_name.clear();
	}
}

pub struct XdsStore {
	pub targets: TargetStore,
	pub policies: PolicyStore,
}

impl XdsStore {
	pub fn new() -> Self {
		Self {
			targets: TargetStore::new(),
			policies: PolicyStore::new(),
		}
	}
}
