use std::{
	cmp,
	collections::{hash_map::Entry, BTreeMap, HashMap, HashSet},
	fmt::Debug,
	sync::Arc,
	time::{Duration, Instant},
};

use base64::{engine::general_purpose, Engine as _};
pub use data::Data;
use federation::transactions::send_transaction_message;
use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::{
	api::{
		appservice::Registration,
		federation::{
			self,
			transactions::edu::{
				DeviceListUpdateContent, Edu, PresenceContent, PresenceUpdate, ReceiptContent, ReceiptData, ReceiptMap,
			},
		},
		OutgoingRequest,
	},
	device_id,
	events::{push_rules::PushRulesEvent, receipt::ReceiptType, AnySyncEphemeralRoomEvent, GlobalAccountDataEventType},
	push, uint, MilliSecondsSinceUnixEpoch, OwnedServerName, OwnedUserId, RoomId, ServerName, UInt, UserId,
};
use tokio::sync::{oneshot, Mutex, Semaphore};
use tracing::{error, warn};

use crate::{service::presence::Presence, services, utils::calculate_hash, Config, Error, PduEvent, Result};

pub mod appservice;
pub mod data;
pub mod send;
pub use send::FedDest;

const SELECT_EDU_LIMIT: usize = 16;

pub struct Service {
	db: &'static dyn Data,

	/// The state for a given state hash.
	pub(super) maximum_requests: Arc<Semaphore>,
	pub sender: loole::Sender<(OutgoingDestination, SendingEventType, Vec<u8>)>,
	receiver: Mutex<loole::Receiver<(OutgoingDestination, SendingEventType, Vec<u8>)>>,
	startup_netburst: bool,
	startup_netburst_keep: i64,
	timeout: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingDestination {
	Appservice(String),
	Push(OwnedUserId, String), // user and pushkey
	Normal(OwnedServerName),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[allow(clippy::module_name_repetitions)]
pub enum SendingEventType {
	Pdu(Vec<u8>), // pduid
	Edu(Vec<u8>), // pdu json
	Flush,        // none
}

enum TransactionStatus {
	/// Currently running (for the first time)
	Running,
	/// Failed, backing off for a retry
	Failed {
		failures: u32,
		waker: Option<oneshot::Sender<()>>,
	},
	/// Currently retrying
	Retrying {
		/// number of times failed
		failures: u32,
	},
}

/// A control-flow enum to dictate what the handler should do after (trying to)
/// prepare a transaction
enum TransactionPrepOutcome {
	Send(Vec<SendingEventType>),
	Wake(OutgoingDestination),
	Nothing,
}

impl Service {
	pub fn build(db: &'static dyn Data, config: &Config) -> Arc<Self> {
		let (sender, receiver) = loole::unbounded();
		Arc::new(Self {
			db,
			sender,
			receiver: Mutex::new(receiver),
			maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
			startup_netburst: config.startup_netburst,
			startup_netburst_keep: config.startup_netburst_keep,
			timeout: config.sender_timeout,
		})
	}

	#[tracing::instrument(skip(self, pdu_id, user, pushkey))]
	pub fn send_pdu_push(&self, pdu_id: &[u8], user: &UserId, pushkey: String) -> Result<()> {
		let outgoing_kind = OutgoingDestination::Push(user.to_owned(), pushkey);
		let event = SendingEventType::Pdu(pdu_id.to_owned());
		let _cork = services().globals.db.cork()?;
		let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
		self.sender
			.send((outgoing_kind, event, keys.into_iter().next().unwrap()))
			.unwrap();

		Ok(())
	}

	#[tracing::instrument(skip(self))]
	pub fn send_pdu_appservice(&self, appservice_id: String, pdu_id: Vec<u8>) -> Result<()> {
		let outgoing_kind = OutgoingDestination::Appservice(appservice_id);
		let event = SendingEventType::Pdu(pdu_id);
		let _cork = services().globals.db.cork()?;
		let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
		self.sender
			.send((outgoing_kind, event, keys.into_iter().next().unwrap()))
			.unwrap();

		Ok(())
	}

	#[tracing::instrument(skip(self, room_id, pdu_id))]
	pub fn send_pdu_room(&self, room_id: &RoomId, pdu_id: &[u8]) -> Result<()> {
		let servers = services()
			.rooms
			.state_cache
			.room_servers(room_id)
			.filter_map(Result::ok)
			.filter(|server| &**server != services().globals.server_name());

		self.send_pdu_servers(servers, pdu_id)
	}

	#[tracing::instrument(skip(self, servers, pdu_id))]
	pub fn send_pdu_servers<I: Iterator<Item = OwnedServerName>>(&self, servers: I, pdu_id: &[u8]) -> Result<()> {
		let requests = servers
			.into_iter()
			.map(|server| (OutgoingDestination::Normal(server), SendingEventType::Pdu(pdu_id.to_owned())))
			.collect::<Vec<_>>();
		let _cork = services().globals.db.cork()?;
		let keys = self.db.queue_requests(
			&requests
				.iter()
				.map(|(o, e)| (o, e.clone()))
				.collect::<Vec<_>>(),
		)?;
		for ((outgoing_kind, event), key) in requests.into_iter().zip(keys) {
			self.sender
				.send((outgoing_kind.clone(), event, key))
				.unwrap();
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, server, serialized))]
	pub fn send_edu_server(&self, server: &ServerName, serialized: Vec<u8>) -> Result<()> {
		let outgoing_kind = OutgoingDestination::Normal(server.to_owned());
		let event = SendingEventType::Edu(serialized);
		let _cork = services().globals.db.cork()?;
		let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
		self.sender
			.send((outgoing_kind, event, keys.into_iter().next().unwrap()))
			.unwrap();

		Ok(())
	}

	#[tracing::instrument(skip(self, room_id, serialized))]
	pub fn send_edu_room(&self, room_id: &RoomId, serialized: Vec<u8>) -> Result<()> {
		let servers = services()
			.rooms
			.state_cache
			.room_servers(room_id)
			.filter_map(Result::ok)
			.filter(|server| &**server != services().globals.server_name());

		self.send_edu_servers(servers, serialized)
	}

	#[tracing::instrument(skip(self, servers, serialized))]
	pub fn send_edu_servers<I: Iterator<Item = OwnedServerName>>(&self, servers: I, serialized: Vec<u8>) -> Result<()> {
		let requests = servers
			.into_iter()
			.map(|server| (OutgoingDestination::Normal(server), SendingEventType::Edu(serialized.clone())))
			.collect::<Vec<_>>();
		let _cork = services().globals.db.cork()?;
		let keys = self.db.queue_requests(
			&requests
				.iter()
				.map(|(o, e)| (o, e.clone()))
				.collect::<Vec<_>>(),
		)?;
		for ((outgoing_kind, event), key) in requests.into_iter().zip(keys) {
			self.sender
				.send((outgoing_kind.clone(), event, key))
				.unwrap();
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, room_id))]
	pub fn flush_room(&self, room_id: &RoomId) -> Result<()> {
		let servers = services()
			.rooms
			.state_cache
			.room_servers(room_id)
			.filter_map(Result::ok)
			.filter(|server| &**server != services().globals.server_name());

		self.flush_servers(servers)
	}

	#[tracing::instrument(skip(self, servers))]
	pub fn flush_servers<I: Iterator<Item = OwnedServerName>>(&self, servers: I) -> Result<()> {
		let requests = servers.into_iter().map(OutgoingDestination::Normal);

		for outgoing_kind in requests {
			self.sender
				.send((outgoing_kind, SendingEventType::Flush, Vec::<u8>::new()))
				.unwrap();
		}

		Ok(())
	}

	/// Cleanup event data
	/// Used for instance after we remove an appservice registration
	#[tracing::instrument(skip(self))]
	pub fn cleanup_events(&self, appservice_id: String) -> Result<()> {
		self.db
			.delete_all_requests_for(&OutgoingDestination::Appservice(appservice_id))?;

		Ok(())
	}

	#[tracing::instrument(skip(self, request), name = "request")]
	pub async fn send_federation_request<T>(&self, dest: &ServerName, request: T) -> Result<T::IncomingResponse>
	where
		T: OutgoingRequest + Debug,
	{
		let permit = self.maximum_requests.acquire().await;
		let timeout = Duration::from_secs(self.timeout);
		let client = &services().globals.client.federation;
		let response = tokio::time::timeout(timeout, send::send_request(client, dest, request))
			.await
			.map_err(|_| {
				warn!("Timeout after 300 seconds waiting for server response of {dest}");
				Error::BadServerResponse("Timeout after 300 seconds waiting for server response")
			})?;
		drop(permit);

		response
	}

	/// Sends a request to an appservice
	///
	/// Only returns None if there is no url specified in the appservice
	/// registration file
	pub async fn send_appservice_request<T>(
		&self, registration: Registration, request: T,
	) -> Result<Option<T::IncomingResponse>>
	where
		T: OutgoingRequest + Debug,
	{
		let permit = self.maximum_requests.acquire().await;
		let response = appservice::send_request(registration, request).await;
		drop(permit);

		response
	}

	pub fn start_handler(self: &Arc<Self>) {
		let self2 = Arc::clone(self);
		tokio::spawn(async move {
			self2
				.handler()
				.await
				.expect("Failed to initialize request sending handler");
		});
	}

	#[tracing::instrument(skip(self), name = "sender")]
	async fn handler(&self) -> Result<()> {
		let new_transactions = self.receiver.lock().await;
		let (waking_sender, waking_receiver) = loole::unbounded();

		let mut outgoing = FuturesUnordered::new();
		let mut retrying = FuturesUnordered::new();

		let mut current_transaction_status = HashMap::<OutgoingDestination, TransactionStatus>::new();

		// Retry requests we could not finish yet
		if self.startup_netburst {
			let mut initial_transactions = HashMap::<OutgoingDestination, Vec<SendingEventType>>::new();
			for (key, outgoing_kind, event) in self.db.active_requests().filter_map(Result::ok) {
				let entry = initial_transactions
					.entry(outgoing_kind.clone())
					.or_default();

				if self.startup_netburst_keep >= 0
					&& entry.len() >= usize::try_from(self.startup_netburst_keep).unwrap()
				{
					warn!("Dropping unsent event {:?} {:?}", outgoing_kind, String::from_utf8_lossy(&key),);
					self.db.delete_active_request(key)?;
					continue;
				}

				entry.push(event);
			}

			for (outgoing_kind, events) in initial_transactions {
				current_transaction_status.insert(outgoing_kind.clone(), TransactionStatus::Running);
				outgoing.push(handle_events(outgoing_kind.clone(), events));
			}
		}

		loop {
			tokio::select! {
				Some(response) = outgoing.next() => {
					// Outgoing transaction succeeded
					match response {
						Ok(outgoing_kind) => {
							let _cork = services().globals.db.cork();
							self.db.delete_all_active_requests_for(&outgoing_kind)?;

							// Find events that have been added since starting the last request
							let new_events = self
								.db
								.queued_requests(&outgoing_kind)
								.filter_map(Result::ok)
								.take(30).collect::<Vec<_>>();

							if !new_events.is_empty() {
								// Insert pdus we found
								self.db.mark_as_active(&new_events)?;

								// Clear retries
								current_transaction_status.insert(outgoing_kind.clone(), TransactionStatus::Running);

								outgoing.push(handle_events(
									outgoing_kind,
									new_events.into_iter().map(|(event, _)| event).collect(),
								));
							} else {
								current_transaction_status.remove(&outgoing_kind);
							}
						}
						// Outgoing transaction failed
						Err((destination, err)) => {
							// Set status to Failed, create timer
							let timer = Self::mark_failed_and_backoff(&mut current_transaction_status, destination.clone());

							// Add timer to loop
							retrying.push(timer);

							warn!("Outgoing request to {destination} failed: {err}");
						}
					};
				},

				// Transaction retry timers firing
				Some(dest) = retrying.next() => {
					// Transition Failed => Retrying, return pending old transaction events
					match self.select_events(
						&dest,
						vec![], // will be ignored because fresh == false
						&mut current_transaction_status,
						false,
					) {
						Ok(TransactionPrepOutcome::Send(events)) => {
							outgoing.push(handle_events(dest, events));
						}
						Ok(_) => {
							// Unreachable because fresh == false
							unreachable!("select_events on a stale transaction {} did not return ::Send", dest)
						}

						Err(err) => {
							error!("Ignoring error in (stale) outgoing request ({}) handler: {}", dest, err);

							// transaction dropped, so drop destination as well.
							current_transaction_status.remove(&dest);
						}
					}
				},

				// Explicit wakeups, makes a backoff timer return immediately
				Ok(outgoing) = waking_receiver.recv_async() => {
					if let Some(TransactionStatus::Failed { waker, .. }) = current_transaction_status.get_mut(&outgoing) {
						if let Some(waker) = waker.take() {
							_ = waker.send(());
						}
					}
				},

				// New transactions to be sent out (from server/user activity)
				event = new_transactions.recv_async() => {
					if let Ok((dest, event, key)) = event {
					match self.select_events(
						&dest,
						vec![(event, key)],
						&mut current_transaction_status,
						true) {
						Ok(TransactionPrepOutcome::Send(events)) => {
							outgoing.push(handle_events(dest, events));
						},
						Ok(TransactionPrepOutcome::Wake(dest)) => {
							waking_sender.send(dest).expect("nothing closes this channel but ourselves");
						},
						Ok(TransactionPrepOutcome::Nothing) => {},
						Err(err) => {
							error!("Ignoring error in (fresh) outgoing request ({}) handler: {}", dest, err);
						}
						}
					}
				}
			}
		}
	}

	/// Generates timer/oneshot, alters status to reflect Failed
	///
	/// Returns timer/oneshot future to wake up loop for next retry
	fn mark_failed_and_backoff(
		status: &mut HashMap<OutgoingDestination, TransactionStatus>, dest: OutgoingDestination,
	) -> impl std::future::Future<Output = OutgoingDestination> {
		let now = Instant::now();

		let entry = status
			.get_mut(&dest)
			.expect("guaranteed to be set before this function");

		let failures = match entry {
			// Running -> Failed
			TransactionStatus::Running => 1,
			// Retrying -> Failed
			TransactionStatus::Retrying {
				failures,
			} => *failures + 1,

			// The transition of Failed -> Retrying is handled by handle_events
			TransactionStatus::Failed {
				..
			} => {
				unreachable!(
					"TransactionStatus in inconsistent state: Expected either Running or Retrying, got Failed, \
					 bailing..."
				)
			},
		};

		const ONE_DAY: Duration = Duration::from_secs(60 * 60 * 24);

		// Exponential backoff, clamp upper value to one day
		let next_wakeup = now + (Duration::from_secs(30) * failures * failures).min(ONE_DAY);

		let (fut, waker) = dest.wrap_in_interruptible_sleep(next_wakeup);

		*entry = TransactionStatus::Failed {
			failures,
			waker: Some(waker),
		};

		fut
	}

	/// This prepares a transaction, checks the transaction state, and selects
	/// appropriate events.
	#[tracing::instrument(skip(self, outgoing_kind, new_events, current_transaction_status))]
	fn select_events(
		&self,
		outgoing_kind: &OutgoingDestination,
		new_events: Vec<(SendingEventType, Vec<u8>)>, // Events we want to send: event and full key
		current_transaction_status: &mut HashMap<OutgoingDestination, TransactionStatus>,
		fresh: bool, // Wether or not this transaction came from server activity.
	) -> Result<TransactionPrepOutcome> {
		let (allow, retry, wake_up) =
			self.select_events_current(outgoing_kind.clone(), current_transaction_status, fresh)?;

		// Nothing can be done for this remote, bail out.
		if wake_up {
			return Ok(TransactionPrepOutcome::Wake(outgoing_kind.clone()));
		} else if !allow {
			return Ok(TransactionPrepOutcome::Nothing);
		}

		let _cork = services().globals.db.cork();
		let mut events = Vec::new();

		// Must retry any previous transaction for this remote.
		if retry {
			// We retry the previous transaction
			for (_, e) in self
				.db
				.active_requests_for(outgoing_kind)
				.filter_map(Result::ok)
			{
				events.push(e);
			}
		}

		// Compose the next transaction
		let _cork = services().globals.db.cork();
		if !new_events.is_empty() {
			self.db.mark_as_active(&new_events)?;
			for (e, _) in new_events {
				events.push(e);
			}
		}

		// Add EDU's into the transaction
		if let OutgoingDestination::Normal(server_name) = outgoing_kind {
			if let Ok((select_edus, last_count)) = self.select_edus(server_name) {
				events.extend(select_edus.into_iter().map(SendingEventType::Edu));
				self.db.set_latest_educount(server_name, last_count)?;
			}
		}

		Ok(TransactionPrepOutcome::Send(events))
	}

	#[tracing::instrument(skip(self, outgoing_kind, current_transaction_status))]
	fn select_events_current(
		&self, outgoing_kind: OutgoingDestination,
		current_transaction_status: &mut HashMap<OutgoingDestination, TransactionStatus>, fresh: bool,
	) -> Result<(bool, bool, bool)> {
		let (mut allow, mut retry, mut wake_up) = (true, false, false);

		let entry = current_transaction_status.entry(outgoing_kind);

		if fresh {
			// If its fresh, we initialise the status if we need to.
			//
			// We do nothing if it is already running or retrying.
			//
			// We return with a wake if it is in the Failed state.
			entry
				.and_modify(|e| match e {
					TransactionStatus::Running
					| TransactionStatus::Retrying {
						..
					} => {
						allow = false; // already running
					},
					TransactionStatus::Failed {
						..
					} => {
						// currently sleeping
						wake_up = true;
					},
				})
				.or_insert(TransactionStatus::Running);
		} else {
			// If it's not fresh, we expect an entry.
			//
			// We also expect us to be the only one who are touching this destination right
			// now, and its a stale transaction, so it must be in the Failed state
			match entry {
				Entry::Occupied(mut e) => {
					let e = e.get_mut();
					match e {
						TransactionStatus::Failed {
							failures,
							..
						} => {
							*e = TransactionStatus::Retrying {
								failures: *failures,
							};
							retry = true;
						},

						_ => unreachable!(
							"Encountered bad state when preparing stale transaction: expected Failed state, got \
							 Running or Retrying"
						),
					}
				},
				Entry::Vacant(_) => unreachable!(
					"Encountered bad state when preparing stale transaction: expected Failed state, got vacant state"
				),
			}
		}

		Ok((allow, retry, wake_up))
	}

	#[tracing::instrument(skip(self, server_name))]
	pub fn select_edus(&self, server_name: &ServerName) -> Result<(Vec<Vec<u8>>, u64)> {
		// u64: count of last edu
		let since = self.db.get_latest_educount(server_name)?;
		let mut events = Vec::new();
		let mut max_edu_count = since;
		let mut device_list_changes = HashSet::new();

		for room_id in services().rooms.state_cache.server_rooms(server_name) {
			let room_id = room_id?;
			// Look for device list updates in this room
			device_list_changes.extend(
				services()
					.users
					.keys_changed(room_id.as_ref(), since, None)
					.filter_map(Result::ok)
					.filter(|user_id| user_id.server_name() == services().globals.server_name()),
			);

			if services().globals.allow_outgoing_read_receipts()
				&& !select_edus_receipts(&room_id, since, &mut max_edu_count, &mut events)?
			{
				break;
			}
		}

		for user_id in device_list_changes {
			// Empty prev id forces synapse to resync; because synapse resyncs,
			// we can just insert placeholder data
			let edu = Edu::DeviceListUpdate(DeviceListUpdateContent {
				user_id,
				device_id: device_id!("placeholder").to_owned(),
				device_display_name: Some("Placeholder".to_owned()),
				stream_id: uint!(1),
				prev_id: Vec::new(),
				deleted: None,
				keys: None,
			});

			events.push(serde_json::to_vec(&edu).expect("json can be serialized"));
		}

		if services().globals.allow_outgoing_presence() {
			select_edus_presence(server_name, since, &mut max_edu_count, &mut events)?;
		}

		Ok((events, max_edu_count))
	}
}

/// Look for presence
#[tracing::instrument(skip(server_name, since, max_edu_count, events))]
pub fn select_edus_presence(
	server_name: &ServerName, since: u64, max_edu_count: &mut u64, events: &mut Vec<Vec<u8>>,
) -> Result<bool> {
	// Look for presence updates for this server
	let mut presence_updates = Vec::new();
	for (user_id, count, presence_bytes) in services().presence.presence_since(since) {
		*max_edu_count = cmp::max(count, *max_edu_count);

		if user_id.server_name() != services().globals.server_name() {
			continue;
		}

		if !services()
			.rooms
			.state_cache
			.server_sees_user(server_name, &user_id)?
		{
			continue;
		}

		let presence_event = Presence::from_json_bytes_to_event(&presence_bytes, &user_id)?;
		presence_updates.push(PresenceUpdate {
			user_id,
			presence: presence_event.content.presence,
			currently_active: presence_event.content.currently_active.unwrap_or(false),
			last_active_ago: presence_event
				.content
				.last_active_ago
				.unwrap_or_else(|| uint!(0)),
			status_msg: presence_event.content.status_msg,
		});

		if presence_updates.len() >= SELECT_EDU_LIMIT {
			break;
		}
	}

	let presence_content = Edu::Presence(PresenceContent::new(presence_updates));
	events.push(serde_json::to_vec(&presence_content).expect("PresenceEvent can be serialized"));

	Ok(true)
}

/// Look for read receipts in this room
#[tracing::instrument(skip(room_id, since, max_edu_count, events))]
pub fn select_edus_receipts(
	room_id: &RoomId, since: u64, max_edu_count: &mut u64, events: &mut Vec<Vec<u8>>,
) -> Result<bool> {
	for r in services()
		.rooms
		.read_receipt
		.readreceipts_since(room_id, since)
	{
		let (user_id, count, read_receipt) = r?;
		*max_edu_count = cmp::max(count, *max_edu_count);

		if user_id.server_name() != services().globals.server_name() {
			continue;
		}

		let event = serde_json::from_str(read_receipt.json().get())
			.map_err(|_| Error::bad_database("Invalid edu event in read_receipts."))?;
		let federation_event = if let AnySyncEphemeralRoomEvent::Receipt(r) = event {
			let mut read = BTreeMap::new();

			let (event_id, mut receipt) = r
				.content
				.0
				.into_iter()
				.next()
				.expect("we only use one event per read receipt");
			let receipt = receipt
				.remove(&ReceiptType::Read)
				.expect("our read receipts always set this")
				.remove(&user_id)
				.expect("our read receipts always have the user here");

			read.insert(
				user_id,
				ReceiptData {
					data: receipt.clone(),
					event_ids: vec![event_id.clone()],
				},
			);

			let receipt_map = ReceiptMap {
				read,
			};

			let mut receipts = BTreeMap::new();
			receipts.insert(room_id.to_owned(), receipt_map);

			Edu::Receipt(ReceiptContent {
				receipts,
			})
		} else {
			Error::bad_database("Invalid event type in read_receipts");
			continue;
		};

		events.push(serde_json::to_vec(&federation_event).expect("json can be serialized"));

		if events.len() >= SELECT_EDU_LIMIT {
			return Ok(false);
		}
	}

	Ok(true)
}

async fn handle_events(
	kind: OutgoingDestination, events: Vec<SendingEventType>,
) -> Result<OutgoingDestination, (OutgoingDestination, Error)> {
	match kind {
		OutgoingDestination::Appservice(ref id) => handle_events_kind_appservice(&kind, id, events).await,
		OutgoingDestination::Push(ref userid, ref pushkey) => {
			handle_events_kind_push(&kind, userid, pushkey, events).await
		},
		OutgoingDestination::Normal(ref server) => handle_events_kind_normal(&kind, server, events).await,
	}
}

#[tracing::instrument(skip(kind, events))]
async fn handle_events_kind_appservice(
	kind: &OutgoingDestination, id: &String, events: Vec<SendingEventType>,
) -> Result<OutgoingDestination, (OutgoingDestination, Error)> {
	let mut pdu_jsons = Vec::new();

	for event in &events {
		match event {
			SendingEventType::Pdu(pdu_id) => {
				pdu_jsons.push(
					services()
						.rooms
						.timeline
						.get_pdu_from_id(pdu_id)
						.map_err(|e| (kind.clone(), e))?
						.ok_or_else(|| {
							(
								kind.clone(),
								Error::bad_database("[Appservice] Event in servernameevent_data not found in db."),
							)
						})?
						.to_room_event(),
				);
			},
			SendingEventType::Edu(_) | SendingEventType::Flush => {
				// Appservices don't need EDUs (?) and flush only;
				// no new content
			},
		}
	}

	let permit = services().sending.maximum_requests.acquire().await;

	let response = match appservice::send_request(
		services()
			.appservice
			.get_registration(id)
			.await
			.ok_or_else(|| {
				(
					kind.clone(),
					Error::bad_database("[Appservice] Could not load registration from db."),
				)
			})?,
		ruma::api::appservice::event::push_events::v1::Request {
			events: pdu_jsons,
			txn_id: (&*general_purpose::URL_SAFE_NO_PAD.encode(calculate_hash(
				&events
					.iter()
					.map(|e| match e {
						SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
						SendingEventType::Flush => &[],
					})
					.collect::<Vec<_>>(),
			)))
				.into(),
		},
	)
	.await
	{
		Ok(_) => Ok(kind.clone()),
		Err(e) => Err((kind.clone(), e)),
	};

	drop(permit);

	response
}

#[tracing::instrument(skip(kind, events))]
async fn handle_events_kind_push(
	kind: &OutgoingDestination, userid: &OwnedUserId, pushkey: &String, events: Vec<SendingEventType>,
) -> Result<OutgoingDestination, (OutgoingDestination, Error)> {
	let mut pdus = Vec::new();

	for event in &events {
		match event {
			SendingEventType::Pdu(pdu_id) => {
				pdus.push(
					services()
						.rooms
						.timeline
						.get_pdu_from_id(pdu_id)
						.map_err(|e| (kind.clone(), e))?
						.ok_or_else(|| {
							(
								kind.clone(),
								Error::bad_database("[Push] Event in servernamevent_datas not found in db."),
							)
						})?,
				);
			},
			SendingEventType::Edu(_) | SendingEventType::Flush => {
				// Push gateways don't need EDUs (?) and flush only;
				// no new content
			},
		}
	}

	for pdu in pdus {
		// Redacted events are not notification targets (we don't send push for them)
		if let Some(unsigned) = &pdu.unsigned {
			if let Ok(unsigned) = serde_json::from_str::<serde_json::Value>(unsigned.get()) {
				if unsigned.get("redacted_because").is_some() {
					continue;
				}
			}
		}

		let Some(pusher) = services()
			.pusher
			.get_pusher(userid, pushkey)
			.map_err(|e| (OutgoingDestination::Push(userid.clone(), pushkey.clone()), e))?
		else {
			continue;
		};

		let rules_for_user = services()
			.account_data
			.get(None, userid, GlobalAccountDataEventType::PushRules.to_string().into())
			.unwrap_or_default()
			.and_then(|event| serde_json::from_str::<PushRulesEvent>(event.get()).ok())
			.map_or_else(|| push::Ruleset::server_default(userid), |ev: PushRulesEvent| ev.content.global);

		let unread: UInt = services()
			.rooms
			.user
			.notification_count(userid, &pdu.room_id)
			.map_err(|e| (kind.clone(), e))?
			.try_into()
			.expect("notification count can't go that high");

		let permit = services().sending.maximum_requests.acquire().await;

		let _response = services()
			.pusher
			.send_push_notice(userid, unread, &pusher, rules_for_user, &pdu)
			.await
			.map(|_response| kind.clone())
			.map_err(|e| (kind.clone(), e));

		drop(permit);
	}

	Ok(kind.clone())
}

#[tracing::instrument(skip(kind, events), name = "")]
async fn handle_events_kind_normal(
	kind: &OutgoingDestination, dest: &OwnedServerName, events: Vec<SendingEventType>,
) -> Result<OutgoingDestination, (OutgoingDestination, Error)> {
	let mut edu_jsons = Vec::new();
	let mut pdu_jsons = Vec::new();

	for event in &events {
		match event {
			SendingEventType::Pdu(pdu_id) => {
				// TODO: check room version and remove event_id if needed
				let raw = PduEvent::convert_to_outgoing_federation_event(
					services()
						.rooms
						.timeline
						.get_pdu_json_from_id(pdu_id)
						.map_err(|e| (kind.clone(), e))?
						.ok_or_else(|| {
							error!("event not found: {dest} {pdu_id:?}");
							(
								kind.clone(),
								Error::bad_database("[Normal] Event in servernamevent_datas not found in db."),
							)
						})?,
				);
				pdu_jsons.push(raw);
			},
			SendingEventType::Edu(edu) => {
				if let Ok(raw) = serde_json::from_slice(edu) {
					edu_jsons.push(raw);
				}
			},
			SendingEventType::Flush => {
				// flush only; no new content
			},
		}
	}

	let permit = services().sending.maximum_requests.acquire().await;
	let client = &services().globals.client.sender;
	let response = send::send_request(
		client,
		dest,
		send_transaction_message::v1::Request {
			origin: services().globals.server_name().to_owned(),
			pdus: pdu_jsons,
			edus: edu_jsons,
			origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
			transaction_id: (&*general_purpose::URL_SAFE_NO_PAD.encode(calculate_hash(
				&events
					.iter()
					.map(|e| match e {
						SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
						SendingEventType::Flush => &[],
					})
					.collect::<Vec<_>>(),
			)))
				.into(),
		},
	)
	.await
	.map(|response| {
		for pdu in response.pdus {
			if pdu.1.is_err() {
				warn!("error for {} from remote: {:?}", pdu.0, pdu.1);
			}
		}
		kind.clone()
	})
	.map_err(|e| (kind.clone(), e));

	drop(permit);

	response
}

impl OutgoingDestination {
	#[tracing::instrument(skip(self))]
	pub fn get_prefix(&self) -> Vec<u8> {
		let mut prefix = match self {
			OutgoingDestination::Appservice(server) => {
				let mut p = b"+".to_vec();
				p.extend_from_slice(server.as_bytes());
				p
			},
			OutgoingDestination::Push(user, pushkey) => {
				let mut p = b"$".to_vec();
				p.extend_from_slice(user.as_bytes());
				p.push(0xFF);
				p.extend_from_slice(pushkey.as_bytes());
				p
			},
			OutgoingDestination::Normal(server) => {
				let mut p = Vec::new();
				p.extend_from_slice(server.as_bytes());
				p
			},
		};
		prefix.push(0xFF);

		prefix
	}

	/// This wraps the OutgoingDestination key in an interruptible sleep future.
	///
	/// The first return value is the future, the second is the oneshot that
	/// interrupts that future, and causes it to return instantly.
	fn wrap_in_interruptible_sleep(
		self, at: Instant,
	) -> (impl std::future::Future<Output = Self>, oneshot::Sender<()>) {
		let (tx, rx) = oneshot::channel();
		let at = tokio::time::Instant::from_std(at);

		(
			async move {
				_ = tokio::time::timeout_at(at, rx).await;

				self
			},
			tx,
		)
	}
}

impl std::fmt::Display for OutgoingDestination {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			OutgoingDestination::Appservice(appservice_id) => {
				write!(f, "Appservice (ID {:?})", appservice_id)
			},
			OutgoingDestination::Push(user, push_key) => {
				write!(f, "User Push Service (for {:?}, with key {:?})", user, push_key)
			},
			OutgoingDestination::Normal(server) => {
				write!(f, "Matrix Server ({:?})", server)
			},
		}
	}
}
