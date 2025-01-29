use std::{
	cmp::{self},
	collections::{hash_map::Entry, BTreeMap, HashMap, HashSet},
	time::Duration,
};

use axum::extract::State;
use conduwuit::{
	at, err, error, extract_variant, is_equal_to,
	pdu::EventHash,
	result::FlatOk,
	utils::{
		self,
		future::OptionExt,
		math::ruma_from_u64,
		stream::{BroadbandExt, Tools, WidebandExt},
		BoolExt, IterStream, ReadyExt, TryFutureExtExt,
	},
	Error, PduCount, PduEvent, Result,
};
use conduwuit_service::{
	rooms::{
		lazy_loading,
		lazy_loading::{Options, Witness},
		short::ShortStateHash,
	},
	Services,
};
use futures::{
	future::{join, join3, join4, join5, try_join, try_join4, OptionFuture},
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
};
use ruma::{
	api::client::{
		filter::FilterDefinition,
		sync::sync_events::{
			self,
			v3::{
				Ephemeral, Filter, GlobalAccountData, InviteState, InvitedRoom, JoinedRoom,
				KnockState, KnockedRoom, LeftRoom, Presence, RoomAccountData, RoomSummary, Rooms,
				State as RoomState, Timeline, ToDevice,
			},
			DeviceLists, UnreadNotificationsCount,
		},
		uiaa::UiaaResponse,
	},
	events::{
		presence::PresenceEvent,
		room::member::{MembershipState, RoomMemberEventContent},
		AnyRawAccountDataEvent, AnySyncEphemeralRoomEvent, StateEventType,
		TimelineEventType::*,
	},
	serde::Raw,
	uint, DeviceId, EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
};

use super::{load_timeline, share_encrypted_room};
use crate::{client::ignored_filter, Ruma, RumaResponse};

#[derive(Default)]
struct StateChanges {
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	joined_since_last_sync: bool,
	state_events: Vec<PduEvent>,
}

type PresenceUpdates = HashMap<OwnedUserId, PresenceEvent>;

/// # `GET /_matrix/client/r0/sync`
///
/// Synchronize the client's state with the latest state on the server.
///
/// - This endpoint takes a `since` parameter which should be the `next_batch`
///   value from a previous request for incremental syncs.
///
/// Calling this endpoint without a `since` parameter returns:
/// - Some of the most recent events of each timeline
/// - Notification counts for each room
/// - Joined and invited member counts, heroes
/// - All state events
///
/// Calling this endpoint with a `since` parameter from a previous `next_batch`
/// returns: For joined rooms:
/// - Some of the most recent events of each timeline that happened after since
/// - If user joined the room after since: All state events (unless lazy loading
///   is activated) and all device list updates in that room
/// - If the user was already in the room: A list of all events that are in the
///   state now, but were not in the state at `since`
/// - If the state we send contains a member event: Joined and invited member
///   counts, heroes
/// - Device list updates that happened after `since`
/// - If there are events in the timeline we send or the user send updated his
///   read mark: Notification counts
/// - EDUs that are active now (read receipts, typing updates, presence)
/// - TODO: Allow multiple sync streams to support Pantalaimon
///
/// For invited rooms:
/// - If the user was invited after `since`: A subset of the state of the room
///   at the point of the invite
///
/// For left rooms:
/// - If the user left after `since`: `prev_batch` token, empty state (TODO:
///   subset of the state at the point of the leave)
#[tracing::instrument(
	name = "sync",
	level = "debug",
	skip_all,
	fields(
		since = %body.body.since.as_deref().unwrap_or_default(),
    )
)]
pub(crate) async fn sync_events_route(
	State(services): State<crate::State>,
	body: Ruma<sync_events::v3::Request>,
) -> Result<sync_events::v3::Response, RumaResponse<UiaaResponse>> {
	let (sender_user, sender_device) = body.sender();

	// Presence update
	if services.globals.allow_local_presence() {
		services
			.presence
			.ping_presence(sender_user, &body.body.set_presence)
			.await?;
	}

	// Setup watchers, so if there's no response, we can wait for them
	let watcher = services.sync.watch(sender_user, sender_device);

	let response = build_sync_events(&services, &body).await?;
	if body.body.full_state
		|| !(response.rooms.is_empty()
			&& response.presence.is_empty()
			&& response.account_data.is_empty()
			&& response.device_lists.is_empty()
			&& response.to_device.is_empty())
	{
		return Ok(response);
	}

	// Hang a few seconds so requests are not spammed
	// Stop hanging if new info arrives
	let default = Duration::from_secs(30);
	let duration = cmp::min(body.body.timeout.unwrap_or(default), default);
	_ = tokio::time::timeout(duration, watcher).await;

	// Retry returning data
	build_sync_events(&services, &body).await
}

pub(crate) async fn build_sync_events(
	services: &Services,
	body: &Ruma<sync_events::v3::Request>,
) -> Result<sync_events::v3::Response, RumaResponse<UiaaResponse>> {
	let (sender_user, sender_device) = body.sender();

	let next_batch = services.globals.current_count()?;
	let since = body
		.body
		.since
		.as_ref()
		.and_then(|string| string.parse().ok())
		.unwrap_or(0);

	let full_state = body.body.full_state;
	let filter = match body.body.filter.as_ref() {
		| None => FilterDefinition::default(),
		| Some(Filter::FilterDefinition(ref filter)) => filter.clone(),
		| Some(Filter::FilterId(ref filter_id)) => services
			.users
			.get_filter(sender_user, filter_id)
			.await
			.unwrap_or_default(),
	};

	let joined_rooms = services
		.rooms
		.state_cache
		.rooms_joined(sender_user)
		.map(ToOwned::to_owned)
		.broad_filter_map(|room_id| {
			load_joined_room(
				services,
				sender_user,
				sender_device,
				room_id.clone(),
				since,
				next_batch,
				full_state,
				&filter,
			)
			.map_ok(move |(joined_room, dlu, jeu)| (room_id, joined_room, dlu, jeu))
			.ok()
		})
		.ready_fold(
			(BTreeMap::new(), HashSet::new(), HashSet::new()),
			|(mut joined_rooms, mut device_list_updates, mut left_encrypted_users),
			 (room_id, joined_room, dlu, leu)| {
				device_list_updates.extend(dlu);
				left_encrypted_users.extend(leu);
				if !joined_room.is_empty() {
					joined_rooms.insert(room_id, joined_room);
				}

				(joined_rooms, device_list_updates, left_encrypted_users)
			},
		);

	let left_rooms = services
		.rooms
		.state_cache
		.rooms_left(sender_user)
		.broad_filter_map(|(room_id, _)| {
			handle_left_room(
				services,
				since,
				room_id.clone(),
				sender_user,
				next_batch,
				full_state,
				&filter,
			)
			.map_ok(move |left_room| (room_id, left_room))
			.ok()
		})
		.ready_filter_map(|(room_id, left_room)| left_room.map(|left_room| (room_id, left_room)))
		.collect();

	let invited_rooms = services
		.rooms
		.state_cache
		.rooms_invited(sender_user)
		.fold_default(|mut invited_rooms: BTreeMap<_, _>, (room_id, invite_state)| async move {
			let invite_count = services
				.rooms
				.state_cache
				.get_invite_count(&room_id, sender_user)
				.await
				.ok();

			// Invited before last sync
			if Some(since) >= invite_count {
				return invited_rooms;
			}

			let invited_room = InvitedRoom {
				invite_state: InviteState { events: invite_state },
			};

			invited_rooms.insert(room_id, invited_room);
			invited_rooms
		});

	let knocked_rooms = services
		.rooms
		.state_cache
		.rooms_knocked(sender_user)
		.fold_default(|mut knocked_rooms: BTreeMap<_, _>, (room_id, knock_state)| async move {
			let knock_count = services
				.rooms
				.state_cache
				.get_knock_count(&room_id, sender_user)
				.await
				.ok();

			// Knocked before last sync
			if Some(since) >= knock_count {
				return knocked_rooms;
			}

			let knocked_room = KnockedRoom {
				knock_state: KnockState { events: knock_state },
			};

			knocked_rooms.insert(room_id, knocked_room);
			knocked_rooms
		});

	let presence_updates: OptionFuture<_> = services
		.globals
		.allow_local_presence()
		.then(|| process_presence_updates(services, since, sender_user))
		.into();

	let account_data = services
		.account_data
		.changes_since(None, sender_user, since)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Global))
		.collect();

	// Look for device list updates of this account
	let keys_changed = services
		.users
		.keys_changed(sender_user, since, None)
		.map(ToOwned::to_owned)
		.collect::<HashSet<_>>();

	let to_device_events = services
		.users
		.get_to_device_events(sender_user, sender_device)
		.collect::<Vec<_>>();

	let device_one_time_keys_count = services
		.users
		.count_one_time_keys(sender_user, sender_device);

	// Remove all to-device events the device received *last time*
	let remove_to_device_events =
		services
			.users
			.remove_to_device_events(sender_user, sender_device, since);

	let rooms = join4(joined_rooms, left_rooms, invited_rooms, knocked_rooms);
	let ephemeral = join3(remove_to_device_events, to_device_events, presence_updates);
	let top = join5(account_data, ephemeral, device_one_time_keys_count, keys_changed, rooms)
		.boxed()
		.await;

	let (account_data, ephemeral, device_one_time_keys_count, keys_changed, rooms) = top;
	let ((), to_device_events, presence_updates) = ephemeral;
	let (joined_rooms, left_rooms, invited_rooms, knocked_rooms) = rooms;
	let (joined_rooms, mut device_list_updates, left_encrypted_users) = joined_rooms;
	device_list_updates.extend(keys_changed);

	// If the user doesn't share an encrypted room with the target anymore, we need
	// to tell them
	let device_list_left = left_encrypted_users
		.into_iter()
		.stream()
		.broad_filter_map(|user_id| async move {
			let no_shared_encrypted_room =
				!share_encrypted_room(services, sender_user, &user_id, None).await;
			no_shared_encrypted_room.then_some(user_id)
		})
		.ready_fold(HashSet::new(), |mut device_list_left, user_id| {
			device_list_left.insert(user_id);
			device_list_left
		})
		.await;

	let response = sync_events::v3::Response {
		account_data: GlobalAccountData { events: account_data },
		device_lists: DeviceLists {
			changed: device_list_updates.into_iter().collect(),
			left: device_list_left.into_iter().collect(),
		},
		device_one_time_keys_count,
		// Fallback keys are not yet supported
		device_unused_fallback_key_types: None,
		next_batch: next_batch.to_string(),
		presence: Presence {
			events: presence_updates
				.unwrap_or_default()
				.into_values()
				.map(|v| Raw::new(&v).expect("PresenceEvent always serializes successfully"))
				.collect(),
		},
		rooms: Rooms {
			leave: left_rooms,
			join: joined_rooms,
			invite: invited_rooms,
			knock: knocked_rooms,
		},
		to_device: ToDevice { events: to_device_events },
	};

	Ok(response)
}

#[tracing::instrument(name = "presence", level = "debug", skip_all)]
async fn process_presence_updates(
	services: &Services,
	since: u64,
	syncing_user: &UserId,
) -> PresenceUpdates {
	services
		.presence
		.presence_since(since)
		.filter(|(user_id, ..)| {
			services
				.rooms
				.state_cache
				.user_sees_user(syncing_user, user_id)
		})
		.filter_map(|(user_id, _, presence_bytes)| {
			services
				.presence
				.from_json_bytes_to_event(presence_bytes, user_id)
				.map_ok(move |event| (user_id, event))
				.ok()
		})
		.ready_fold(PresenceUpdates::new(), |mut updates, (user_id, event)| {
			match updates.entry(user_id.into()) {
				| Entry::Vacant(slot) => {
					let mut new_event = event;
					new_event.content.last_active_ago = match new_event.content.currently_active {
						| Some(true) => None,
						| _ => new_event.content.last_active_ago,
					};

					slot.insert(new_event);
				},
				| Entry::Occupied(mut slot) => {
					let curr_event = slot.get_mut();
					let curr_content = &mut curr_event.content;
					let new_content = event.content;

					// Update existing presence event with more info
					curr_content.presence = new_content.presence;
					curr_content.status_msg = new_content
						.status_msg
						.or_else(|| curr_content.status_msg.take());
					curr_content.displayname = new_content
						.displayname
						.or_else(|| curr_content.displayname.take());
					curr_content.avatar_url = new_content
						.avatar_url
						.or_else(|| curr_content.avatar_url.take());
					curr_content.currently_active = new_content
						.currently_active
						.or(curr_content.currently_active);
					curr_content.last_active_ago = match curr_content.currently_active {
						| Some(true) => None,
						| _ => new_content.last_active_ago.or(curr_content.last_active_ago),
					};
				},
			};

			updates
		})
		.await
}

#[tracing::instrument(
	name = "left",
	level = "debug",
	skip_all,
	fields(
		room_id = %room_id,
		full = %full_state,
	),
)]
#[allow(clippy::too_many_arguments)]
async fn handle_left_room(
	services: &Services,
	since: u64,
	ref room_id: OwnedRoomId,
	sender_user: &UserId,
	next_batch: u64,
	full_state: bool,
	filter: &FilterDefinition,
) -> Result<Option<LeftRoom>> {
	let left_count = services
		.rooms
		.state_cache
		.get_left_count(room_id, sender_user)
		.await
		.ok();

	// Left before last sync
	if Some(since) >= left_count {
		return Ok(None);
	}

	if !services.rooms.metadata.exists(room_id).await {
		// This is just a rejected invite, not a room we know
		// Insert a leave event anyways
		let event = PduEvent {
			event_id: EventId::new(services.globals.server_name()),
			sender: sender_user.to_owned(),
			origin: None,
			origin_server_ts: utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
			kind: RoomMember,
			content: serde_json::from_str(r#"{"membership":"leave"}"#)
				.expect("this is valid JSON"),
			state_key: Some(sender_user.to_string()),
			unsigned: None,
			// The following keys are dropped on conversion
			room_id: room_id.clone(),
			prev_events: vec![],
			depth: uint!(1),
			auth_events: vec![],
			redacts: None,
			hashes: EventHash { sha256: String::new() },
			signatures: None,
		};

		return Ok(Some(LeftRoom {
			account_data: RoomAccountData { events: Vec::new() },
			timeline: Timeline {
				limited: false,
				prev_batch: Some(next_batch.to_string()),
				events: Vec::new(),
			},
			state: RoomState {
				events: vec![event.to_sync_state_event()],
			},
		}));
	}

	let mut left_state_events = Vec::new();

	let since_shortstatehash = services.rooms.user.get_token_shortstatehash(room_id, since);

	let since_state_ids: HashMap<_, OwnedEventId> = since_shortstatehash
		.map_ok(|since_shortstatehash| {
			services
				.rooms
				.state_accessor
				.state_full_ids(since_shortstatehash)
				.map(Ok)
		})
		.try_flatten_stream()
		.try_collect()
		.await
		.unwrap_or_default();

	let Ok(left_event_id): Result<OwnedEventId> = services
		.rooms
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomMember, sender_user.as_str())
		.await
	else {
		error!("Left room but no left state event");
		return Ok(None);
	};

	let Ok(left_shortstatehash) = services
		.rooms
		.state_accessor
		.pdu_shortstatehash(&left_event_id)
		.await
	else {
		error!(event_id = %left_event_id, "Leave event has no state");
		return Ok(None);
	};

	let mut left_state_ids: HashMap<_, _> = services
		.rooms
		.state_accessor
		.state_full_ids(left_shortstatehash)
		.collect()
		.await;

	let leave_shortstatekey = services
		.rooms
		.short
		.get_or_create_shortstatekey(&StateEventType::RoomMember, sender_user.as_str())
		.await;

	left_state_ids.insert(leave_shortstatekey, left_event_id);

	for (shortstatekey, event_id) in left_state_ids {
		if full_state || since_state_ids.get(&shortstatekey) != Some(&event_id) {
			let (event_type, state_key) = services
				.rooms
				.short
				.get_statekey_from_short(shortstatekey)
				.await?;

			if filter.room.state.lazy_load_options.is_enabled()
				&& event_type == StateEventType::RoomMember
				&& !full_state
				&& state_key
					.as_str()
					.try_into()
					.is_ok_and(|user_id: &UserId| sender_user != user_id)
			{
				continue;
			}

			let Ok(pdu) = services.rooms.timeline.get_pdu(&event_id).await else {
				error!("Pdu in state not found: {event_id}");
				continue;
			};

			left_state_events.push(pdu.to_sync_state_event());
		}
	}

	Ok(Some(LeftRoom {
		account_data: RoomAccountData { events: Vec::new() },
		timeline: Timeline {
			// TODO: support left timeline events so we dont need to set limited to true
			limited: true,
			prev_batch: Some(next_batch.to_string()),
			events: Vec::new(), // and so we dont need to set this to empty vec
		},
		state: RoomState { events: left_state_events },
	}))
}

#[tracing::instrument(
	name = "joined",
	level = "debug",
	skip_all,
	fields(
		room_id = ?room_id,
	),
)]
#[allow(clippy::too_many_arguments)]
async fn load_joined_room(
	services: &Services,
	sender_user: &UserId,
	sender_device: &DeviceId,
	ref room_id: OwnedRoomId,
	since: u64,
	next_batch: u64,
	full_state: bool,
	filter: &FilterDefinition,
) -> Result<(JoinedRoom, HashSet<OwnedUserId>, HashSet<OwnedUserId>)> {
	let sincecount = PduCount::Normal(since);
	let next_batchcount = PduCount::Normal(next_batch);

	let current_shortstatehash = services
		.rooms
		.state
		.get_room_shortstatehash(room_id)
		.map_err(|_| err!(Database(error!("Room {room_id} has no state"))));

	let since_shortstatehash = services
		.rooms
		.user
		.get_token_shortstatehash(room_id, since)
		.ok()
		.map(Ok);

	let timeline = load_timeline(
		services,
		sender_user,
		room_id,
		sincecount,
		Some(next_batchcount),
		10_usize,
	);

	let receipt_events = services
		.rooms
		.read_receipt
		.readreceipts_since(room_id, since)
		.filter_map(|(read_user, _, edu)| async move {
			services
				.users
				.user_is_ignored(read_user, sender_user)
				.await
				.or_some((read_user.to_owned(), edu))
		})
		.collect::<HashMap<OwnedUserId, Raw<AnySyncEphemeralRoomEvent>>>()
		.map(Ok);

	let (current_shortstatehash, since_shortstatehash, timeline, receipt_events) =
		try_join4(current_shortstatehash, since_shortstatehash, timeline, receipt_events)
			.boxed()
			.await?;

	let (timeline_pdus, limited) = timeline;

	let last_notification_read: OptionFuture<_> = timeline_pdus
		.is_empty()
		.then(|| {
			services
				.rooms
				.user
				.last_notification_read(sender_user, room_id)
		})
		.into();

	let no_state_changes = timeline_pdus.is_empty()
		&& (since_shortstatehash.is_none()
			|| since_shortstatehash.is_some_and(is_equal_to!(current_shortstatehash)));

	let since_sender_member: OptionFuture<_> = since_shortstatehash
		.map(|short| {
			services
				.rooms
				.state_accessor
				.state_get_content(short, &StateEventType::RoomMember, sender_user.as_str())
				.ok()
		})
		.into();

	let joined_since_last_sync =
		since_sender_member
			.await
			.flatten()
			.is_none_or(|content: RoomMemberEventContent| {
				content.membership != MembershipState::Join
			});

	let lazy_loading_enabled = filter.room.state.lazy_load_options.is_enabled()
		|| filter.room.timeline.lazy_load_options.is_enabled();

	let generate_witness =
		lazy_loading_enabled && (since_shortstatehash.is_none() || joined_since_last_sync);

	let lazy_reset = lazy_loading_enabled && since_shortstatehash.is_none();

	let lazy_loading_context = &lazy_loading::Context {
		user_id: sender_user,
		device_id: sender_device,
		room_id,
		token: None,
		options: Some(&filter.room.state.lazy_load_options),
	};

	// Reset lazy loading because this is an initial sync
	let lazy_load_reset: OptionFuture<_> = lazy_reset
		.then(|| services.rooms.lazy_loading.reset(lazy_loading_context))
		.into();

	lazy_load_reset.await;
	let witness: Option<Witness> = generate_witness.then(|| {
		timeline_pdus
			.iter()
			.map(|(_, pdu)| pdu.sender.clone())
			.chain(receipt_events.keys().cloned())
			.collect()
	});

	let witness: OptionFuture<_> = witness
		.map(|witness| {
			services
				.rooms
				.lazy_loading
				.witness_retain(witness, lazy_loading_context)
		})
		.into();

	let witness = witness.await;
	let mut device_list_updates = HashSet::<OwnedUserId>::new();
	let mut left_encrypted_users = HashSet::<OwnedUserId>::new();
	let StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		joined_since_last_sync,
		state_events,
	} = if no_state_changes {
		StateChanges::default()
	} else {
		calculate_state_changes(
			services,
			sender_user,
			room_id,
			full_state,
			filter,
			&mut device_list_updates,
			&mut left_encrypted_users,
			since_shortstatehash,
			current_shortstatehash,
			joined_since_last_sync,
			witness.as_ref(),
		)
		.boxed()
		.await?
	};

	let account_data_events = services
		.account_data
		.changes_since(Some(room_id), sender_user, since)
		.ready_filter_map(|e| extract_variant!(e, AnyRawAccountDataEvent::Room))
		.collect();

	// Look for device list updates in this room
	let device_updates = services
		.users
		.room_keys_changed(room_id, since, None)
		.map(|(user_id, _)| user_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>();

	let room_events = timeline_pdus
		.iter()
		.stream()
		.wide_filter_map(|item| ignored_filter(services, item.clone(), sender_user))
		.map(|(_, pdu)| pdu.to_sync_room_event())
		.collect();

	let typing_events = services
		.rooms
		.typing
		.last_typing_update(room_id)
		.and_then(|count| async move {
			if count <= since {
				return Ok(Vec::<Raw<AnySyncEphemeralRoomEvent>>::new());
			}

			let typings = services
				.rooms
				.typing
				.typings_user(room_id, sender_user)
				.await;

			Ok(vec![serde_json::from_str(&serde_json::to_string(&typings)?)?])
		})
		.unwrap_or(Vec::new());

	let send_notification_counts = last_notification_read
		.is_none_or(|&count| count > since)
		.await;

	let notification_count: OptionFuture<_> = send_notification_counts
		.then(|| {
			services
				.rooms
				.user
				.notification_count(sender_user, room_id)
				.map(TryInto::try_into)
				.unwrap_or(uint!(0))
		})
		.into();

	let highlight_count: OptionFuture<_> = send_notification_counts
		.then(|| {
			services
				.rooms
				.user
				.highlight_count(sender_user, room_id)
				.map(TryInto::try_into)
				.unwrap_or(uint!(0))
		})
		.into();

	let events = join3(room_events, account_data_events, typing_events);
	let unread_notifications = join(notification_count, highlight_count);
	let (unread_notifications, events, device_updates) =
		join3(unread_notifications, events, device_updates)
			.boxed()
			.await;

	let (room_events, account_data_events, typing_events) = events;
	let (notification_count, highlight_count) = unread_notifications;

	device_list_updates.extend(device_updates);

	let last_privateread_update = services
		.rooms
		.read_receipt
		.last_privateread_update(sender_user, room_id)
		.await > since;

	let private_read_event = if last_privateread_update {
		services
			.rooms
			.read_receipt
			.private_read_get(room_id, sender_user)
			.await
			.ok()
	} else {
		None
	};

	let edus: Vec<Raw<AnySyncEphemeralRoomEvent>> = receipt_events
		.into_values()
		.chain(typing_events.into_iter())
		.chain(private_read_event.into_iter())
		.collect();

	// Save the state after this sync so we can send the correct state diff next
	// sync
	services
		.rooms
		.user
		.associate_token_shortstatehash(room_id, next_batch, current_shortstatehash)
		.await;

	let joined_room = JoinedRoom {
		account_data: RoomAccountData { events: account_data_events },
		summary: RoomSummary {
			joined_member_count: joined_member_count.map(ruma_from_u64),
			invited_member_count: invited_member_count.map(ruma_from_u64),
			heroes: heroes
				.into_iter()
				.flatten()
				.map(TryInto::try_into)
				.filter_map(Result::ok)
				.collect(),
		},
		unread_notifications: UnreadNotificationsCount { highlight_count, notification_count },
		timeline: Timeline {
			limited: limited || joined_since_last_sync,
			events: room_events,
			prev_batch: timeline_pdus
				.first()
				.map(at!(0))
				.as_ref()
				.map(ToString::to_string),
		},
		state: RoomState {
			events: state_events
				.iter()
				.map(PduEvent::to_sync_state_event)
				.collect(),
		},
		ephemeral: Ephemeral { events: edus },
		unread_thread_notifications: BTreeMap::new(),
	};

	Ok((joined_room, device_list_updates, left_encrypted_users))
}

#[tracing::instrument(
	name = "state",
	level = "trace",
	skip_all,
	fields(
	    full = %full_state,
	    cs = %current_shortstatehash,
	    ss = ?since_shortstatehash,
    )
)]
#[allow(clippy::too_many_arguments)]
async fn calculate_state_changes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	filter: &FilterDefinition,
	device_list_updates: &mut HashSet<OwnedUserId>,
	left_encrypted_users: &mut HashSet<OwnedUserId>,
	since_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: ShortStateHash,
	joined_since_last_sync: bool,
	witness: Option<&Witness>,
) -> Result<StateChanges> {
	if since_shortstatehash.is_none() || joined_since_last_sync {
		calculate_state_initial(
			services,
			sender_user,
			room_id,
			full_state,
			filter,
			current_shortstatehash,
			witness,
		)
		.await
	} else {
		calculate_state_incremental(
			services,
			sender_user,
			room_id,
			full_state,
			filter,
			device_list_updates,
			left_encrypted_users,
			since_shortstatehash,
			current_shortstatehash,
			joined_since_last_sync,
		)
		.await
	}
}

#[tracing::instrument(name = "initial", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
async fn calculate_state_initial(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	filter: &FilterDefinition,
	current_shortstatehash: ShortStateHash,
	witness: Option<&Witness>,
) -> Result<StateChanges> {
	let (shortstatekeys, event_ids): (Vec<_>, Vec<_>) = services
		.rooms
		.state_accessor
		.state_full_ids(current_shortstatehash)
		.unzip()
		.await;

	let state_events = services
		.rooms
		.short
		.multi_get_statekey_from_short(shortstatekeys.into_iter().stream())
		.zip(event_ids.into_iter().stream())
		.ready_filter_map(|item| Some((item.0.ok()?, item.1)))
		.ready_filter_map(|((event_type, state_key), event_id)| {
			let lazy_load_enabled = filter.room.state.lazy_load_options.is_enabled()
				|| filter.room.timeline.lazy_load_options.is_enabled();

			if lazy_load_enabled
				&& event_type == StateEventType::RoomMember
				&& !full_state
				&& state_key.as_str().try_into().is_ok_and(|user_id: &UserId| {
					sender_user != user_id
						&& witness.is_some_and(|witness| !witness.contains(user_id))
				}) {
				return None;
			}

			Some(event_id)
		})
		.broad_filter_map(|event_id: OwnedEventId| async move {
			services.rooms.timeline.get_pdu(&event_id).await.ok()
		})
		.collect()
		.map(Ok);

	let counts = calculate_counts(services, room_id, sender_user);
	let ((joined_member_count, invited_member_count, heroes), state_events) =
		try_join(counts, state_events).boxed().await?;

	// The state_events above should contain all timeline_users, let's mark them as
	// lazy loaded.

	Ok(StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		joined_since_last_sync: true,
		state_events,
	})
}

#[tracing::instrument(name = "incremental", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
async fn calculate_state_incremental(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	full_state: bool,
	_filter: &FilterDefinition,
	device_list_updates: &mut HashSet<OwnedUserId>,
	left_encrypted_users: &mut HashSet<OwnedUserId>,
	since_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: ShortStateHash,
	joined_since_last_sync: bool,
) -> Result<StateChanges> {
	// Incremental /sync
	let since_shortstatehash =
		since_shortstatehash.expect("missing since_shortstatehash on incremental sync");

	let mut delta_state_events = Vec::new();

	if since_shortstatehash != current_shortstatehash {
		let current_state_ids = services
			.rooms
			.state_accessor
			.state_full_ids(current_shortstatehash)
			.collect();

		let since_state_ids = services
			.rooms
			.state_accessor
			.state_full_ids(since_shortstatehash)
			.collect();

		let (current_state_ids, since_state_ids): (
			HashMap<_, OwnedEventId>,
			HashMap<_, OwnedEventId>,
		) = join(current_state_ids, since_state_ids).await;

		current_state_ids
			.iter()
			.stream()
			.ready_filter(|(key, id)| full_state || since_state_ids.get(key) != Some(id))
			.wide_filter_map(|(_, id)| services.rooms.timeline.get_pdu(id).ok())
			.ready_for_each(|pdu| delta_state_events.push(pdu))
			.await;
	}

	let encrypted_room = services
		.rooms
		.state_accessor
		.state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")
		.is_ok();

	let since_encryption = services
		.rooms
		.state_accessor
		.state_get(since_shortstatehash, &StateEventType::RoomEncryption, "")
		.is_ok();

	let (encrypted_room, since_encryption) = join(encrypted_room, since_encryption).await;

	// Calculations:
	let new_encrypted_room = encrypted_room && !since_encryption;

	let send_member_count = delta_state_events
		.iter()
		.any(|event| event.kind == RoomMember);

	if encrypted_room {
		for state_event in &delta_state_events {
			if state_event.kind != RoomMember {
				continue;
			}

			if let Some(state_key) = &state_event.state_key {
				let user_id = UserId::parse(state_key)
					.map_err(|_| Error::bad_database("Invalid UserId in member PDU."))?;

				if user_id == sender_user {
					continue;
				}

				let content: RoomMemberEventContent = state_event.get_content()?;

				match content.membership {
					| MembershipState::Join => {
						// A new user joined an encrypted room
						if !share_encrypted_room(services, sender_user, user_id, Some(room_id))
							.await
						{
							device_list_updates.insert(user_id.into());
						}
					},
					| MembershipState::Leave => {
						// Write down users that have left encrypted rooms we are in
						left_encrypted_users.insert(user_id.into());
					},
					| _ => {},
				}
			}
		}
	}

	if joined_since_last_sync && encrypted_room || new_encrypted_room {
		let updates: Vec<OwnedUserId> = services
			.rooms
			.state_cache
			.room_members(room_id)
			.ready_filter(|user_id| sender_user != *user_id)
			.filter_map(|user_id| {
				share_encrypted_room(services, sender_user, user_id, Some(room_id))
					.map(|res| res.or_some(user_id.to_owned()))
			})
			.collect()
			.await;

		// If the user is in a new encrypted room, give them all joined users
		device_list_updates.extend(updates);
	}

	let (joined_member_count, invited_member_count, heroes) = if send_member_count {
		calculate_counts(services, room_id, sender_user).await?
	} else {
		(None, None, None)
	};

	Ok(StateChanges {
		heroes,
		joined_member_count,
		invited_member_count,
		joined_since_last_sync,
		state_events: delta_state_events,
	})
}

async fn calculate_counts(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Result<(Option<u64>, Option<u64>, Option<Vec<OwnedUserId>>)> {
	let joined_member_count = services
		.rooms
		.state_cache
		.room_joined_count(room_id)
		.unwrap_or(0);

	let invited_member_count = services
		.rooms
		.state_cache
		.room_invited_count(room_id)
		.unwrap_or(0);

	let (joined_member_count, invited_member_count) =
		join(joined_member_count, invited_member_count).await;

	let small_room = joined_member_count.saturating_add(invited_member_count) <= 5;

	let heroes: OptionFuture<_> = small_room
		.then(|| calculate_heroes(services, room_id, sender_user))
		.into();

	Ok((Some(joined_member_count), Some(invited_member_count), heroes.await))
}

async fn calculate_heroes(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Vec<OwnedUserId> {
	services
		.rooms
		.timeline
		.all_pdus(sender_user, room_id)
		.ready_filter(|(_, pdu)| pdu.kind == RoomMember)
		.fold_default(|heroes: Vec<_>, (_, pdu)| {
			fold_hero(heroes, services, room_id, sender_user, pdu)
		})
		.await
}

async fn fold_hero(
	mut heroes: Vec<OwnedUserId>,
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
	pdu: PduEvent,
) -> Vec<OwnedUserId> {
	let Some(user_id): Option<&UserId> =
		pdu.state_key.as_deref().map(TryInto::try_into).flat_ok()
	else {
		return heroes;
	};

	if user_id == sender_user {
		return heroes;
	}

	let Ok(content): Result<RoomMemberEventContent, _> = pdu.get_content() else {
		return heroes;
	};

	// The membership was and still is invite or join
	if !matches!(content.membership, MembershipState::Join | MembershipState::Invite) {
		return heroes;
	}

	if heroes.iter().any(is_equal_to!(user_id)) {
		return heroes;
	}

	let (is_invited, is_joined) = join(
		services.rooms.state_cache.is_invited(user_id, room_id),
		services.rooms.state_cache.is_joined(user_id, room_id),
	)
	.await;

	if !is_joined && is_invited {
		return heroes;
	}

	heroes.push(user_id.to_owned());
	heroes
}
