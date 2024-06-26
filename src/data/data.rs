use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, RwLock},
};

use anyhow::Context;
use ironworks::{
	excel::{Excel, Language},
	sqpack::SqPack,
	zipatch, Ironworks,
};
use serde::Deserialize;
use tokio::{select, sync::watch};
use tokio_util::sync::CancellationToken;

use crate::version::{self, VersionKey};

use super::{
	error::{Error, Result},
	language::LanguageString,
};

#[derive(Debug, Deserialize)]
pub struct Config {
	language: LanguageString,
}

pub struct Data {
	default_language: Language,

	channel: watch::Sender<Vec<VersionKey>>,

	// Root ZiPatch instance, acts as a LUT cache
	zipatch: zipatch::ZiPatch,

	versions: RwLock<HashMap<VersionKey, Arc<Version>>>,
}

impl Data {
	pub fn new(config: Config) -> Self {
		let (sender, _receiver) = watch::channel(vec![]);

		Data {
			default_language: config.language.into(),
			channel: sender,
			zipatch: zipatch::ZiPatch::new().with_persisted_lookups(),
			versions: Default::default(),
		}
	}

	pub fn ready(&self) -> bool {
		// We don't know how many versions there might be in total, but there should
		// be at least one. Mark ready when we have _something_.
		self.versions.read().expect("poisoned").len() > 0
	}

	pub fn default_language(&self) -> Language {
		self.default_language
	}

	pub fn subscribe(&self) -> watch::Receiver<Vec<VersionKey>> {
		self.channel.subscribe()
	}

	pub async fn start(&self, cancel: CancellationToken, version: &version::Manager) -> Result<()> {
		let execute_prepare = |versions: Vec<VersionKey>| async {
			select! {
				result = self.prepare_new_versions(version, versions) => result,
				_ = cancel.cancelled() => Ok(()),
			}
		};

		let mut receiver = version.subscribe();

		execute_prepare(receiver.borrow().clone()).await?;

		loop {
			select! {
				Ok(_) = receiver.changed() => execute_prepare(receiver.borrow().clone()).await?,
				_ = cancel.cancelled() => break,
			}
		}

		Ok(())
	}

	async fn prepare_new_versions(
		&self,
		version: &version::Manager,
		versions: Vec<VersionKey>,
	) -> Result<()> {
		// Filter the incoming version list down to the ones we're not already aware of.
		let known_keys = self
			.versions
			.read()
			.expect("poisoned")
			.keys()
			.cloned()
			.collect::<HashSet<_>>();

		let results = versions
			.into_iter()
			.filter(|key| !known_keys.contains(key))
			.map(|key| {
				self.prepare_version(version, key)
					.map_err(|error| (key, error))
			});

		// Run all the version preparation. We aren't failing fast on this, as an
		// erroneous version should not prevent other versions from being prepared.
		for (key, error) in results.filter_map(Result::err) {
			tracing::warn!(%key, reason = %error, "did not prepare version")
		}

		Ok(())
	}

	fn prepare_version(&self, manager: &version::Manager, version_key: VersionKey) -> Result<()> {
		// Preparation only happens when we're told that a version exists, so anything going wrong _here_ is a hefty failure.
		let version = manager
			.version(version_key)
			.context("version does not exist")?;

		let view = version
			.repositories
			.into_iter()
			.map(|repository| zipatch::PatchRepository {
				patches: repository
					.patches
					.into_iter()
					.map(|patch| zipatch::Patch {
						path: patch.path,
						name: patch.name,
					})
					.collect(),
			})
			.zip(0u8..)
			.fold(self.zipatch.view(), |builder, (repository, index)| {
				builder.with_repository(index, repository)
			})
			.build();

		// Build a version and save it out to the struct.
		let version = Version::new(view);
		self.versions
			.write()
			.expect("poisoned")
			.insert(version_key, Arc::new(version));

		tracing::debug!(key = %version_key, "version prepared");

		// Broadcast the update.
		// NOTE: This is performed after each version rather than when all versions
		// are complete to allow other services to begin processing an early-completing
		// version before the full patch process is complete.
		self.broadcast_version_list();

		Ok(())
	}

	pub fn version(&self, version: VersionKey) -> Result<Arc<Version>> {
		let versions = self.versions.read().expect("poisoned");

		versions
			.get(&version)
			.ok_or_else(|| Error::UnknownVersion(version))
			.cloned()
	}

	fn broadcast_version_list(&self) {
		let versions = self.versions.read().expect("poisoned");
		let keys = versions.keys().copied().collect::<Vec<_>>();

		self.channel.send_if_modified(|value| {
			if &keys != value {
				*value = keys;
				return true;
			}

			false
		});
	}
}

pub struct Version {
	ironworks: Arc<Ironworks>,
	excel: Arc<Excel<'static>>,
}

impl Version {
	fn new(view: zipatch::View) -> Self {
		let ironworks = Arc::new(Ironworks::new().with_resource(SqPack::new(view)));
		let excel = Arc::new(Excel::new(ironworks.clone()));
		Self { ironworks, excel }
	}

	pub fn ironworks(&self) -> Arc<Ironworks> {
		self.ironworks.clone()
	}

	pub fn excel(&self) -> Arc<Excel<'static>> {
		self.excel.clone()
	}
}
