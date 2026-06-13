use ruma::RoomId;

async fn migrate_topological_index(services: &Services) -> Result<()> {
	info!("Starting migration to populate roomid_topologicalorder_pducount...");

	let db = &services.db;
	let room_pducount_eventid = db["room_pducount_eventid"].clone();
	let eventid_metadata = db["eventid_metadata"].clone();
	let roomid_topologicalorder_pducount = db["roomid_topologicalorder_pducount"].clone();
	let eventid_pdu = db["eventid_pdu"].clone();

	let mut stream = room_pducount_eventid.raw_stream();
	pin_mut!(stream);

	let mut total_migrated: usize = 0;
	let mut current_room = Vec::new();

	while let Some((pdu_id_bytes, event_id_bytes)) = stream.try_next().await? {
		let pdu_id: crate::rooms::timeline::RawPduId = pdu_id_bytes.as_ref().into();

		let Ok(pdu) = eventid_pdu.get(&event_id_bytes).await.deserialized::<PduEvent>() else {
			continue;
		};

		let Ok(metadata_bytes) = eventid_metadata.get(&event_id_bytes).await else {
			continue;
		};

		let Ok(mut meta) = bincode::deserialize::<crate::rooms::timeline::EventMetadata>(&metadata_bytes) else {
			continue;
		};

		// Skip if already migrated (assuming depth > 0 means migrated, though 0 is valid? No, depth is at least 1)
		// Wait, if it's already migrated, we still need to populate index. Let's just do it unconditionally.

		let mut max_depth = 0;
		for prev_id in pdu.prev_events() {
			if let Ok(prev_bytes) = eventid_metadata.get_blocking(prev_id.as_bytes()) {
				if let Ok(prev_meta) = bincode::deserialize::<crate::rooms::timeline::EventMetadata>(&prev_bytes) {
					max_depth = max_depth.max(prev_meta.local_topological_depth);
				}
			}
		}

		let local_topological_depth = max_depth + 1;
		meta.local_topological_depth = local_topological_depth;

		if let Ok(new_metadata_bytes) = bincode::serialize(&meta) {
			eventid_metadata.put(&event_id_bytes, new_metadata_bytes);
		}

		let topo_key = crate::rooms::timeline::Data::topo_pducount_key(&pdu_id, local_topological_depth);
		roomid_topologicalorder_pducount.put(&topo_key, event_id_bytes.clone());

		total_migrated = total_migrated.saturating_add(1);
		if total_migrated.is_multiple_of(10000) {
			info!("Migrated {} events to topological index...", total_migrated);
		}
	}

	info!("Successfully populated topological index for {total_migrated} events!");
	db["global"].insert(MIGRATE_TOPOLOGICAL_INDEX_MARKER, []);
	db.db.sort()?;
	Ok(())
}
