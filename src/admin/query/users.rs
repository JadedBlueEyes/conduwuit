use clap::Subcommand;
use conduit::Result;
use futures::stream::StreamExt;
use ruma::{events::room::message::RoomMessageEventContent, OwnedDeviceId, OwnedRoomId, OwnedUserId};

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
/// All the getters and iterators from src/database/key_value/users.rs
pub(crate) enum UsersCommand {
	CountUsers,

	IterUsers,

	PasswordHash {
		user_id: OwnedUserId,
	},

	ListDevices {
		user_id: OwnedUserId,
	},

	ListDevicesMetadata {
		user_id: OwnedUserId,
	},

	GetDeviceMetadata {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
	},

	GetDevicesVersion {
		user_id: OwnedUserId,
	},

	CountOneTimeKeys {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
	},

	GetDeviceKeys {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
	},

	GetUserSigningKey {
		user_id: OwnedUserId,
	},

	GetMasterKey {
		user_id: OwnedUserId,
	},

	GetToDeviceEvents {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
	},

	GetLatestBackup {
		user_id: OwnedUserId,
	},

	GetLatestBackupVersion {
		user_id: OwnedUserId,
	},

	GetBackupAlgorithm {
		user_id: OwnedUserId,
		version: String,
	},

	GetAllBackups {
		user_id: OwnedUserId,
		version: String,
	},

	GetRoomBackups {
		user_id: OwnedUserId,
		version: String,
		room_id: OwnedRoomId,
	},

	GetBackupSession {
		user_id: OwnedUserId,
		version: String,
		room_id: OwnedRoomId,
		session_id: String,
	},
}

#[admin_command]
async fn get_backup_session(
	&self, user_id: OwnedUserId, version: String, room_id: OwnedRoomId, session_id: String,
) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.key_backups
		.get_session(&user_id, &version, &room_id, &session_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_room_backups(
	&self, user_id: OwnedUserId, version: String, room_id: OwnedRoomId,
) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.key_backups
		.get_room(&user_id, &version, &room_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_all_backups(&self, user_id: OwnedUserId, version: String) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self.services.key_backups.get_all(&user_id, &version).await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_backup_algorithm(&self, user_id: OwnedUserId, version: String) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.key_backups
		.get_backup(&user_id, &version)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_latest_backup_version(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.key_backups
		.get_latest_backup_version(&user_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_latest_backup(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self.services.key_backups.get_latest_backup(&user_id).await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn iter_users(&self) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result: Vec<OwnedUserId> = self.services.users.stream().map(Into::into).collect().await;

	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn count_users(&self) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self.services.users.count().await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn password_hash(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self.services.users.password_hash(&user_id).await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn list_devices(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let devices = self
		.services
		.users
		.all_device_ids(&user_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{devices:#?}\n```"
	)))
}

#[admin_command]
async fn list_devices_metadata(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let devices = self
		.services
		.users
		.all_devices_metadata(&user_id)
		.collect::<Vec<_>>()
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{devices:#?}\n```"
	)))
}

#[admin_command]
async fn get_device_metadata(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let device = self
		.services
		.users
		.get_device_metadata(&user_id, &device_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{device:#?}\n```"
	)))
}

#[admin_command]
async fn get_devices_version(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let device = self.services.users.get_devicelist_version(&user_id).await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{device:#?}\n```"
	)))
}

#[admin_command]
async fn count_one_time_keys(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.users
		.count_one_time_keys(&user_id, &device_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_device_keys(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.users
		.get_device_keys(&user_id, &device_id)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_user_signing_key(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self.services.users.get_user_signing_key(&user_id).await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_master_key(&self, user_id: OwnedUserId) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.users
		.get_master_key(None, &user_id, &|_| true)
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}

#[admin_command]
async fn get_to_device_events(
	&self, user_id: OwnedUserId, device_id: OwnedDeviceId,
) -> Result<RoomMessageEventContent> {
	let timer = tokio::time::Instant::now();
	let result = self
		.services
		.users
		.get_to_device_events(&user_id, &device_id)
		.collect::<Vec<_>>()
		.await;
	let query_time = timer.elapsed();

	Ok(RoomMessageEventContent::notice_markdown(format!(
		"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
	)))
}
