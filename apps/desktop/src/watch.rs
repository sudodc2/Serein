//! Desktop ownership for watching other participants' screen shares. Each choice is an
//! explicit click; frames are decoded off the render thread and never written to disk.
use client_core::{Command, Event, State, screen, voice};
use discord_voice::{Identity, Status};
use eframe::egui;
use model::Id;
use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};
use zeroize::Zeroizing;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);
// Eight UHD YUV pictures plus the displayed picture fit without increasing the item bound.
const DECODED_BUDGET: usize = 128 * 1024 * 1024;

/// Latest stream picture and whether the UI still has to upload it.
type Slot = Option<(platform::video::Picture, bool)>;

/// Replace the latest picture in place: a plain copy of the decoder's planes into one reused
/// allocation. Colour conversion happens on the GPU when the UI uploads it.
fn store_frame(slot: &mut Slot, frame: discord_voice::RemoteFrame<'_>) -> bool {
	let picture = frame.picture;
	if !picture.is_valid() {
		return false;
	}
	let len = picture.data.len();
	let mut data = slot.take().map(|(old, _)| old.data).unwrap_or_default();
	if data.capacity() > 1024 * 1024 && data.capacity() > len.saturating_mul(4) {
		// Release a previous large resolution while keeping ordinary resize reuse.
		data = Vec::new();
	}
	data.clear();
	// Exact growth: never double a previous resolution's allocation beyond the frame.
	data.reserve_exact(len);
	data.extend_from_slice(picture.data);
	*slot = Some((picture.with_data(data), true));
	true
}

// Decode early, display on the audio/device clock. Only decoded pictures may be
// skipped here: the H.264 decoder has already consumed their reference state.
struct Scheduled {
	latest: Slot,
	pending: std::collections::VecDeque<(platform::video::Picture, discord_voice::Presentation)>,
	bytes: usize,
	/// Byte bound for this stream's pending queue; shared down while several streams decode.
	budget: usize,
}
impl Default for Scheduled {
	fn default() -> Self {
		Self {
			latest: None,
			pending: std::collections::VecDeque::new(),
			bytes: 0,
			budget: DECODED_BUDGET,
		}
	}
}
impl Scheduled {
	fn store(&mut self, frame: discord_voice::RemoteFrame<'_>) -> bool {
		let Some(presentation) = frame.presentation.clone() else {
			return store_frame(&mut self.latest, frame);
		};
		if !presentation.is_current() || !frame.picture.is_valid() {
			return false;
		}
		let length = frame.picture.data.len();
		if length > self.budget {
			return false;
		}
		let old = if self.latest.as_ref().is_some_and(|(_, fresh)| !*fresh) {
			self.latest.take()
		} else {
			None
		};
		let mut data = old.map(|(picture, _)| picture.data).unwrap_or_default();
		if data.capacity() > length.saturating_mul(4) {
			data = Vec::new();
		}
		let required = data.capacity().max(length);
		while self.pending.len() >= 8
			|| self.bytes
				+ required + self
				.latest
				.as_ref()
				.map_or(0, |(picture, _)| picture.data.capacity())
				> self.budget
		{
			let Some((old, _)) = self.pending.pop_front() else {
				return false;
			};
			self.bytes -= old.data.capacity();
		}
		data.clear();
		data.reserve_exact(length);
		data.extend_from_slice(frame.picture.data);
		self.bytes += data.capacity();
		self.pending
			.push_back((frame.picture.with_data(data), presentation));
		true
	}
	fn advance(&mut self, now: Instant) -> Option<Instant> {
		while let Some((_, stamp)) = self.pending.front() {
			if stamp.is_current() && stamp.deadline(now) > now {
				return Some(stamp.deadline(now));
			}
			let (picture, stamp) = self.pending.pop_front().expect("front exists");
			self.bytes -= picture.data.capacity();
			if stamp.is_current() {
				self.latest = Some((picture, true));
			}
		}
		None
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context {
	generation: u64,
	channel: Id,
	request: u64,
	stream_request: u64,
	streamer: Id,
}
struct Pending {
	context: Context,
	user: Id,
	peer: Option<Id>,
	session: Zeroizing<String>,
	identity: Arc<Identity>,
	audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
	rtc: Option<(Id, Id)>,
	server: Option<(voice::Secret, String)>,
	started: Instant,
	status: &'static str,
}
#[derive(Clone, Copy)]
enum Notice {
	Status(&'static str),
	Failed(&'static str),
}
struct Live {
	context: Context,
	task: JoinHandle<()>,
	events: watch::Receiver<Option<Notice>>,
	frames: Arc<Mutex<Scheduled>>,
	/// GPU texture behind this streamer's `ui.voice_stream_views` entry.
	texture: crate::gpu_video::VideoTexture,
	status: &'static str,
}

#[derive(Default)]
pub(super) struct Watch {
	pending: Vec<Pending>,
	live: Vec<Live>,
	/// A stream the streamer stopped or Discord failed; the state choice is cleared next poll.
	ended: Vec<(Context, ui::design::Level, &'static str)>,
	/// Commands beyond the one a poll returns; drained one per frame.
	outbox: std::collections::VecDeque<Command>,
	sequence: u64,
}
impl Watch {
	pub fn stop(&mut self) {
		self.pending.clear();
		self.ended.clear();
		self.outbox.clear();
		for live in self.live.drain(..) {
			live.task.abort();
		}
	}
	/// Queue a stream command; the poll wrapper returns one command per frame.
	fn queue(&mut self, command: Command) {
		self.outbox.push_back(command);
	}
	/// Tell Discord this device left one stream's view, when the attempt is still current.
	fn release(&mut self, context: Context, state: &State) {
		if context.generation == state.generation && !state.demo {
			self.queue(Command::Voice(voice::Command::StopWatching {
				channel: context.channel,
				request: context.request,
				stream_request: context.stream_request,
			}));
		}
	}
	/// Take negotiation secrets before the UI reduces the event. Nothing is persisted.
	pub fn observe(&mut self, state: &State, event: &mut Event) {
		let Event::Voice(voice::Event::Watch {
			channel,
			request,
			stream_request,
			streamer,
			event,
		}) = event
		else {
			return;
		};
		let matches = |context: &Context| {
			context.generation == state.generation
				&& (
					context.channel,
					context.request,
					context.stream_request,
					context.streamer,
				) == (*channel, *request, *stream_request, *streamer)
		};
		if let Some(index) = self
			.pending
			.iter()
			.position(|pending| matches(&pending.context))
		{
			let context = self.pending[index].context;
			match event {
				screen::Event::Created {
					rtc_server,
					rtc_channel,
				} => {
					self.pending[index].rtc = Some((*rtc_server, *rtc_channel));
				}
				screen::Event::Server { token, endpoint } => {
					match (token.take(), endpoint.take()) {
						(Some(token), Some(endpoint)) => {
							self.pending[index].server = Some((token, endpoint));
						}
						// A null endpoint means Discord is still allocating the stream server.
						(_, None) => self.pending[index].server = None,
						(None, Some(_)) => self.ended.push((
							context,
							ui::design::Level::Warning,
							"Discord omitted the stream connection token",
						)),
					}
				}
				screen::Event::Deleted { reason } => self.ended.push((
					context,
					ui::design::Level::Info,
					reason.unwrap_or("The stream ended"),
				)),
				screen::Event::Failed(message) => {
					self.ended
						.push((context, ui::design::Level::Warning, message));
				}
				screen::Event::Viewers(_) => {}
			}
			return;
		}
		if let Some(live) = self.live.iter().find(|live| matches(&live.context)) {
			let context = live.context;
			match event {
				screen::Event::Deleted { reason } => self.ended.push((
					context,
					ui::design::Level::Info,
					reason.unwrap_or("The stream ended"),
				)),
				screen::Event::Failed(message) => {
					self.ended
						.push((context, ui::design::Level::Warning, message));
				}
				_ => {}
			}
		}
	}
	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		call: Option<super::screen::Call<'_>>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
	) -> Option<Command> {
		self.poll_inner(runtime, state, ui, ctx, call, audio);
		self.outbox.pop_front()
	}
	fn poll_inner(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		call: Option<super::screen::Call<'_>>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
	) {
		// A stream the streamer stopped or Discord failed leaves its watch; the rest continue.
		for (context, level, message) in std::mem::take(&mut self.ended) {
			if let Some(index) = self
				.pending
				.iter()
				.position(|pending| pending.context == context)
			{
				self.pending.remove(index);
			}
			if let Some(index) = self.live.iter().position(|live| live.context == context) {
				self.live.remove(index).task.abort();
			}
			ui.voice_stream_views
				.retain(|(id, _)| *id != context.streamer);
			ui.voice_stream_statuses
				.retain(|(id, _)| *id != context.streamer);
			// Said once; the stage keeps no line for a view that already ended.
			ui.toasts.push(level, message);
			state.stop_watching_streamer(context.streamer);
			self.release(context, state);
			ctx.request_repaint();
		}
		let wanted: Vec<(u64, Id, u64, Id)> = state
			.voice
			.active
			.as_ref()
			.filter(|active| active.phase != voice::Phase::Failed)
			.map(|active| {
				active
					.watching
					.iter()
					.map(|streamer| (state.generation, active.channel, active.request, *streamer))
					.collect()
			})
			.unwrap_or_default();
		let wanted_ids: Vec<Id> = wanted.iter().map(|(_, _, _, streamer)| *streamer).collect();
		// Views always mirror the wanted set, whatever removed a stream; demo textures
		// for watched shares survive the same sweep.
		ui.voice_stream_views
			.retain(|(id, _)| wanted_ids.contains(id));
		let wanted_key = |context: &Context| {
			wanted.contains(&(
				context.generation,
				context.channel,
				context.request,
				context.streamer,
			))
		};
		// Sessions the selection no longer wants release their stream view, one command each.
		let mut index = 0;
		while index < self.pending.len() {
			if wanted_key(&self.pending[index].context) {
				index += 1;
			} else {
				let context = self.pending.remove(index).context;
				self.release(context, state);
			}
		}
		let mut index = 0;
		while index < self.live.len() {
			if wanted_key(&self.live[index].context) {
				index += 1;
			} else {
				let live = self.live.remove(index);
				live.task.abort();
				self.release(live.context, state);
				ui.voice_stream_views
					.retain(|(id, _)| *id != live.context.streamer);
			}
		}
		if state.demo {
			ui.voice_stream_statuses.clear();
			for (_, _, _, streamer) in &wanted {
				ui.voice_stream_statuses
					.push((*streamer, "Offline preview · no stream is received"));
			}
			return;
		}
		// Every wanted streamer without a session starts negotiating; the separate stream
		// transport survives call rekeys and voice resumption.
		let mut blocked = Vec::new();
		for (generation, channel, request, streamer) in wanted {
			let in_flight = self.pending.iter().any(|pending| {
				(
					pending.context.generation,
					pending.context.channel,
					pending.context.request,
					pending.context.streamer,
				) == (generation, channel, request, streamer)
			}) || self.live.iter().any(|live| {
				(
					live.context.generation,
					live.context.channel,
					live.context.request,
					live.context.streamer,
				) == (generation, channel, request, streamer)
			});
			if in_flight {
				continue;
			}
			let Some(call) = call.as_ref().filter(|call| {
				call.generation == generation && call.channel == channel && call.request == request
			}) else {
				blocked.push((streamer, "Connect the call before watching a stream"));
				continue;
			};
			self.sequence = self.sequence.wrapping_add(1);
			let context = Context {
				generation,
				channel,
				request,
				stream_request: self.sequence,
				streamer,
			};
			self.pending.push(Pending {
				context,
				user: call.user,
				peer: call.peer,
				session: Zeroizing::new(call.session.to_owned()),
				identity: call.identity.clone(),
				audio: audio.clone(),
				rtc: None,
				server: None,
				started: Instant::now(),
				status: "Requesting the stream…",
			});
			self.queue(Command::Voice(voice::Command::WatchStream {
				channel,
				request,
				stream_request: context.stream_request,
				streamer,
			}));
		}
		// Negotiation that never arrives ends its own watch; the soonest deadline repaints.
		let mut index = 0;
		let mut repaint = None;
		while index < self.pending.len() {
			let pending = &self.pending[index];
			let elapsed = pending.started.elapsed();
			if elapsed >= SIGNAL_TIMEOUT {
				let context = self.pending.remove(index).context;
				self.ended.push((
					context,
					ui::design::Level::Warning,
					"Discord did not provide the stream connection; try again",
				));
				continue;
			}
			repaint = Some(repaint.map_or(SIGNAL_TIMEOUT - elapsed, |at: Duration| {
				at.min(SIGNAL_TIMEOUT - elapsed)
			}));
			index += 1;
		}
		if let Some(delay) = repaint {
			ctx.request_repaint_after(delay);
		}
		// Negotiated pendings become live stream sessions.
		while let Some(index) = self
			.pending
			.iter()
			.position(|pending| pending.rtc.is_some() && pending.server.is_some())
		{
			let pending = self.pending.remove(index);
			let context = pending.context;
			if let Err(error) = self.start(runtime, pending, ctx) {
				self.ended
					.push((context, ui::design::Level::Warning, error));
			}
		}
		// Each live stream drains its own events, frames and texture; the decode byte
		// budget divides across however many are actually playing.
		let live_count = self.live.len().max(1);
		for live in &mut self.live {
			match *live.events.borrow_and_update() {
				Some(Notice::Status(status)) => live.status = status,
				Some(Notice::Failed(error))
					if !self
						.ended
						.iter()
						.any(|(context, _, _)| *context == live.context) =>
				{
					self.ended
						.push((live.context, ui::design::Level::Warning, error));
				}
				_ => {}
			}
			if live.task.is_finished()
				&& !self
					.ended
					.iter()
					.any(|(context, _, _)| *context == live.context)
			{
				self.ended.push((
					live.context,
					ui::design::Level::Warning,
					"The stream connection ended",
				));
			}
			// Uploading copies the planes into GPU staging, so it runs under the slot lock
			// and the decoder keeps refilling the same allocation.
			if let Ok(mut slot) = live.frames.try_lock() {
				slot.budget = DECODED_BUDGET / live_count;
				if let Some(deadline) = slot.advance(Instant::now()) {
					ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
				}
				if let Some((picture, fresh)) = slot.latest.as_mut()
					&& std::mem::take(fresh)
					&& let Some(texture) =
						live.texture.upload(ctx, "remote-stream", picture.borrow())
				{
					let streamer = live.context.streamer;
					match ui
						.voice_stream_views
						.iter_mut()
						.find(|(id, _)| *id == streamer)
					{
						Some((_, shown)) => *shown = texture,
						None => ui.voice_stream_views.push((streamer, texture)),
					}
					live.status = "Watching the stream";
				}
			}
		}
		// Status lines cover negotiating, live and blocked shares alike.
		ui.voice_stream_statuses.clear();
		for pending in &self.pending {
			ui.voice_stream_statuses
				.push((pending.context.streamer, pending.status));
		}
		for live in &self.live {
			ui.voice_stream_statuses
				.push((live.context.streamer, live.status));
		}
		for (streamer, status) in blocked {
			ui.voice_stream_statuses.push((streamer, status));
		}
	}
	fn start(
		&mut self,
		runtime: &Runtime,
		mut pending: Pending,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let (rtc_server, rtc_channel) = pending.rtc.take().ok_or("Missing stream RTC identity")?;
		let (token, endpoint) = pending.server.take().ok_or("Missing stream server")?;
		let session = voice::Secret::new(pending.session.to_string())
			.map_err(|_| "Invalid stream voice session")?;
		let credentials = voice::VoiceConnection {
			channel: rtc_channel,
			guild: Some(rtc_server),
			user: pending.user,
			peer: pending.peer,
			session,
			token,
			endpoint,
			request: pending.context.stream_request,
		};
		let frames = Arc::new(Mutex::new(Scheduled::default()));
		let slot = frames.clone();
		let wake = ctx.clone();
		let streamer = pending.context.streamer.0;
		let sink: discord_voice::VideoSink = Arc::new(move |frame: discord_voice::RemoteFrame| {
			if frame.user != streamer {
				return;
			}
			if slot.lock().is_ok_and(|mut slot| slot.store(frame)) {
				wake.request_repaint();
			}
		});
		let (send, events) = watch::channel(None);
		let wake = ctx.clone();
		let identity = pending.identity;
		let audio = pending.audio;
		let context = pending.context;
		let task = runtime.spawn(async move {
			let (status_send, status_wake) = (send.clone(), wake.clone());
			let result =
				discord_voice::watch_stream(credentials, identity, sink, audio, move |event| {
					let status = match event {
						Status::Connecting => "Connecting to the stream…",
						Status::Discovering => "Checking the stream network…",
						Status::TransportReady | Status::Securing => "Securing the stream…",
						Status::WaitingForPeer => "Waiting for the streamer…",
						Status::Ready { .. } => "Stream secured · waiting for video",
						Status::RemoteAudio | Status::Speaking(_) | Status::CameraAvailable(_) => {
							return Ok(());
						}
					};
					status_send.send_replace(Some(Notice::Status(status)));
					status_wake.request_repaint();
					Ok(())
				})
				.await;
			if let Err(error) = result {
				send.send_replace(Some(Notice::Failed(error)));
			}
			wake.request_repaint();
		});
		self.live.push(Live {
			context,
			task,
			events,
			frames,
			texture: Default::default(),
			status: "Connecting to the stream…",
		});
		Ok(())
	}
}
impl Drop for Watch {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn watch_survives_call_recovery_until_its_selection_ends() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let mut state = test_support::voice_demo_state();
		state.demo = false;
		state.watch_stream(Id(3)).unwrap();
		let call = state.voice.active.as_ref().unwrap();
		let context = Context {
			generation: state.generation,
			channel: call.channel,
			request: call.request,
			stream_request: 2,
			streamer: Id(3),
		};
		let mut ui = ui::MessagingUi::default();
		let ctx = egui::Context::default();
		for failed in [false, true] {
			state.voice.active.as_mut().unwrap().watching = vec![context.streamer];
			let (_send, events) = watch::channel(None);
			let mut watch = Watch {
				pending: Vec::new(),
				live: vec![Live {
					context,
					task: runtime.spawn(std::future::pending()),
					events,
					frames: Arc::new(Mutex::new(Scheduled::default())),
					texture: Default::default(),
					status: "",
				}],
				ended: Vec::new(),
				outbox: std::collections::VecDeque::new(),
				sequence: 0,
			};
			for phase in [
				voice::Phase::Connected,
				voice::Phase::Securing,
				voice::Phase::ConnectingTransport,
				voice::Phase::OpeningAudio,
				voice::Phase::Waiting,
				voice::Phase::Connected,
			] {
				state.voice.active.as_mut().unwrap().phase = phase;
				assert!(
					watch
						.poll(&runtime, &mut state, &mut ui, &ctx, None, None)
						.is_none()
				);
				assert!(!watch.live.is_empty());
			}
			if failed {
				state.voice.active.as_mut().unwrap().phase = voice::Phase::Failed;
			} else {
				state.stop_watching();
			}
			assert!(matches!(
				watch.poll(&runtime, &mut state, &mut ui, &ctx, None, None),
				Some(Command::Voice(voice::Command::StopWatching {
					channel, request, stream_request: 2,
				})) if channel == context.channel && request == context.request
			));
			assert!(watch.live.is_empty());
		}
	}

	fn frame(width: u32, height: u32, data: &[u8]) -> discord_voice::RemoteFrame<'_> {
		discord_voice::RemoteFrame {
			presentation: None,
			user: 7,
			picture: platform::video::Picture {
				width,
				height,
				layout: platform::video::Layout::I420,
				color: platform::video::Color::WEBRTC,
				rotation: 0,
				data,
			},
		}
	}

	fn i420(width: u32, height: u32, value: u8) -> Vec<u8> {
		vec![value; platform::video::Layout::I420.len(width, height).unwrap()]
	}

	#[test]
	fn watch_reuses_the_frame_buffer_and_flags_only_updates() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, 1))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for value in 0..20 {
			slot.as_mut().unwrap().1 = false;
			assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, value))));
			let (picture, fresh) = slot.as_ref().unwrap();
			assert!(*fresh);
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!(picture.data, i420(2, 2, value));
		}
		// A smaller picture reuses the allocation too.
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 9))));
		let (picture, _) = slot.as_ref().unwrap();
		assert_eq!(picture.data.as_ptr(), planes);
		assert_eq!((picture.width, picture.height), (1, 1));
	}

	#[test]
	fn watch_invalid_frames_preserve_the_last_valid_picture() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 4))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for invalid in [
			frame(0, 1, &[]),
			frame(1, 0, &[]),
			frame(1, 1, &[0; 2]),
			frame(u32::MAX, u32::MAX, &[]),
		] {
			assert!(!store_frame(&mut slot, invalid));
			let (picture, _) = slot.as_ref().unwrap();
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!((picture.width, picture.height), (1, 1));
		}
	}

	#[test]
	fn watch_releases_large_capacity_after_a_resolution_drop() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		assert!(store_frame(&mut slot, frame(16, 16, &i420(16, 16, 0))));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 16 * 16 * 3 / 2);
		assert!(picture.data.capacity() <= 16 * 16 * 6);
	}

	#[test]
	fn watch_resolution_growth_does_not_double_the_capacity() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1600, 900, &i420(1600, 900, 0))
		));
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 1920 * 1080 * 3 / 2);
		assert!(picture.data.capacity() <= 1920 * 1080 * 3 / 2);
	}
	#[test]
	fn decoded_stream_pictures_wait_for_presentation_and_pressure_skips_only_decoded_frames() {
		let now = Instant::now();
		let mut queue = Scheduled::default();
		let pixels = i420(3840, 2160, 4);
		for n in 0..16 {
			let mut picture = frame(3840, 2160, &pixels);
			picture.presentation = Some(discord_voice::Presentation::new(
				now + Duration::from_millis(100 + n * 16),
			));
			assert!(queue.store(picture));
		}
		assert_eq!(queue.pending.len(), 8);
		assert!(queue.bytes <= DECODED_BUDGET);
		assert!(queue.latest.is_none());
		assert_eq!(queue.advance(now), Some(now + Duration::from_millis(228)));
		queue.advance(now + Duration::from_millis(260));
		assert_eq!(queue.pending.len(), 5);
		assert!(queue.latest.as_ref().unwrap().1);
		assert!(queue.bytes + queue.latest.as_ref().unwrap().0.data.capacity() <= DECODED_BUDGET);
		drop(queue);
		// A backend's RGBA fallback reaches the byte bound before the eight-picture bound.
		let mut queue = Scheduled::default();
		let pixels = vec![4; 3840 * 2160 * 4];
		for n in 0..16 {
			let mut picture = frame(3840, 2160, &pixels);
			picture.picture.layout = platform::video::Layout::Rgba;
			picture.presentation = Some(discord_voice::Presentation::new(
				now + Duration::from_millis(100 + n * 16),
			));
			assert!(queue.store(picture));
		}
		assert_eq!(queue.pending.len(), 4);
		assert!(queue.bytes <= DECODED_BUDGET);
		queue.advance(now + Duration::from_secs(1));
		assert!(queue.pending.is_empty());
		assert_eq!(queue.bytes, 0);
		assert!(queue.latest.as_ref().unwrap().1);
	}
}
