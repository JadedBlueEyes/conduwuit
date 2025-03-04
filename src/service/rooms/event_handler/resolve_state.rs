use std::{
	borrow::Borrow,
	collections::{HashMap, HashSet},
	sync::Arc,
};

use conduwuit::{
	debug, err, implement,
	utils::stream::{automatic_width, IterStream, ReadyExt, TryWidebandExt, WidebandExt},
	Result,
};
use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{
	state_res::{self, StateMap},
	OwnedEventId, RoomId, RoomVersionId,
};

use crate::rooms::state_compressor::CompressedStateEvent;

#[implement(super::Service)]
#[tracing::instrument(name = "resolve", level = "debug", skip_all)]
pub async fn resolve_state(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	incoming_state: HashMap<u64, OwnedEventId>,
) -> Result<Arc<HashSet<CompressedStateEvent>>> {
	debug!("Loading current room state ids");
	let current_sstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.map_err(|e| err!(Database(error!("No state for {room_id:?}: {e:?}"))))?;

	let current_state_ids: HashMap<_, _> = self
		.services
		.state_accessor
		.state_full_ids(current_sstatehash)
		.collect()
		.await;

	let fork_states = [current_state_ids, incoming_state];
	let auth_chain_sets: Vec<HashSet<OwnedEventId>> = fork_states
		.iter()
		.try_stream()
		.wide_and_then(|state| async move {
			let starting_events = state.values().map(Borrow::borrow);

			let auth_chain = self
				.services
				.auth_chain
				.get_event_ids(room_id, starting_events)
				.await?
				.into_iter()
				.collect();

			Ok(auth_chain)
		})
		.try_collect()
		.await?;

	debug!("Loading fork states");
	let fork_states: Vec<StateMap<OwnedEventId>> = fork_states
		.into_iter()
		.stream()
		.wide_then(|fork_state| async move {
			let shortstatekeys = fork_state.keys().copied().stream();

			let event_ids = fork_state.values().cloned().stream().boxed();

			self.services
				.short
				.multi_get_statekey_from_short(shortstatekeys)
				.zip(event_ids)
				.ready_filter_map(|(ty_sk, id)| Some((ty_sk.ok()?, id)))
				.collect()
				.await
		})
		.collect()
		.await;

	debug!("Resolving state");
	let state = self
		.state_resolution(room_version_id, &fork_states, &auth_chain_sets)
		.boxed()
		.await?;

	debug!("State resolution done.");
	let state_events: Vec<_> = state
		.iter()
		.stream()
		.wide_then(|((event_type, state_key), event_id)| {
			self.services
				.short
				.get_or_create_shortstatekey(event_type, state_key)
				.map(move |shortstatekey| (shortstatekey, event_id))
		})
		.collect()
		.await;

	debug!("Compressing state...");
	let new_room_state: HashSet<_> = self
		.services
		.state_compressor
		.compress_state_events(
			state_events
				.iter()
				.map(|(ref ssk, eid)| (ssk, (*eid).borrow())),
		)
		.collect()
		.await;

	Ok(Arc::new(new_room_state))
}

#[implement(super::Service)]
#[tracing::instrument(name = "ruma", level = "debug", skip_all)]
pub async fn state_resolution(
	&self,
	room_version: &RoomVersionId,
	state_sets: &[StateMap<OwnedEventId>],
	auth_chain_sets: &[HashSet<OwnedEventId>],
) -> Result<StateMap<OwnedEventId>> {
	state_res::resolve(
		room_version,
		state_sets.iter(),
		auth_chain_sets,
		&|event_id| self.event_fetch(event_id),
		&|event_id| self.event_exists(event_id),
		automatic_width(),
	)
	.await
	.map_err(|e| err!(error!("State resolution failed: {e:?}")))
}
