use std::{
	cmp::Reverse,
	collections::BinaryHeap,
	sync::atomic::Ordering,
	time::{Duration, SystemTime},
};

use conduwuit::{Result, err, utils::stream::TryIgnore};
use futures::StreamExt;
use loole::Receiver;
use tokio::{select, time::sleep};

use super::{DELAY_ID_SIZE, ScheduledDelayedEvent, Service};

pub(crate) struct SubmissionQueue {
	receiver: Receiver<(SystemTime, String)>,
	queue: BinaryHeap<(Reverse<SystemTime>, String)>,
}

impl SubmissionQueue {
	pub(crate) fn new(receiver: Receiver<(SystemTime, String)>) -> Self {
		Self { receiver, queue: BinaryHeap::new() }
	}
}

#[allow(clippy::arithmetic_side_effects)]
pub(crate) async fn worker(service: &Service) -> Result<()> {
	let mut queue = service
		.submission_queue
		.try_lock()
		.map_err(|_| err!(Err("Attempted to launch multiple instances of the worker")))?;
	let receiver = queue.receiver.clone();

	let mut scheduled_events = service.scheduled_events.lock().await;
	let mut stream = service
		.db
		.delayid_scheduleddelayedevent
		.stream::<'_, String, ScheduledDelayedEvent>()
		.ignore_err();

	while let Some((delay_id, event)) = stream.next().await {
		let submission_time = event.running_since + event.delay;
		queue
			.queue
			.push((Reverse(submission_time), delay_id.clone()));
		scheduled_events.insert(delay_id);
	}

	drop(scheduled_events);

	// work loop

	loop {
		if service.interrupt_requested.load(Ordering::Relaxed) {
			break;
		}

		let mem_usage = queue.queue.len() * (DELAY_ID_SIZE + size_of::<SystemTime>());
		service.mem_usage.store(mem_usage, Ordering::Relaxed);

		let next_submit = async {
			let (time, _) = queue.queue.peek()?;
			if let Ok(sleep_duration) = time.0.duration_since(SystemTime::now()) {
				sleep(sleep_duration).await;
			}
			let (_, delay_id) = queue.queue.pop()?;
			Some(delay_id)
		};

		let next_receive = receiver.recv_async();

		// RescvFuture is cancellation-safe
		select! {
			Some(delay_id) = next_submit => {
				if let Some((time, delay_id)) = service.send_event_if_ready(delay_id).await {
					queue.queue.push((Reverse(time), delay_id));
				}
			},
			Ok((time, delay_id)) = next_receive => {
				queue.queue.push((Reverse(time), delay_id));
			},
			// Loop regularly to check if the service needs to stop even when there are no in-flight delayed events
			() = sleep(Duration::from_secs(2)) => (),
		}
	}

	Ok(())
}
