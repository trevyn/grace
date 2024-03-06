#![forbid(unsafe_code)]
#![allow(unused_imports, dead_code)]
// #![feature(let_chains)]

use async_openai::types::ChatCompletionRequestMessage;
use async_openai::types::Role::{self, *};
use bytes::{BufMut, Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;
use crossbeam::channel::RecvError;
use deepgram::transcription::live::{Alternatives, Channel, Word as LiveWord};
use deepgram::transcription::prerecorded::response::Word as PrerecordedWord;
use deepgram::{
	transcription::prerecorded::{
		audio_source::AudioSource,
		options::{Language, Options},
	},
	Deepgram, DeepgramError,
};
use egui::text::LayoutJob;
use egui::*;
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::stream::StreamExt as _;
use futures::SinkExt;
use once_cell::sync::Lazy;
use poll_promise::Promise;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::{env, string};
use stream_cancel::{StreamExt as _, Trigger, Tripwire};
use tokio::fs::File;
use turbosql::{execute, now_ms, select, update, Blob, Turbosql};

mod audiofile;
mod self_update;
mod session;

enum Word {
	Live(LiveWord),
	Prerecorded(PrerecordedWord),
}

impl Word {
	fn speaker(&self) -> u8 {
		match self {
			Word::Live(word) => word.speaker,
			Word::Prerecorded(word) => word.speaker.unwrap() as u8,
		}
	}
	fn word(&self) -> &str {
		match self {
			Word::Live(word) => &word.word,
			Word::Prerecorded(word) => &word.word,
		}
	}
}

static TRANSCRIPT: Lazy<Mutex<Vec<Option<LiveWord>>>> = Lazy::new(Default::default);
static TRANSCRIPT_FINAL: Lazy<Mutex<Vec<Option<Word>>>> = Lazy::new(Default::default);
static DURATION: Lazy<Mutex<f64>> = Lazy::new(Default::default);
static COMPLETION: Lazy<Mutex<String>> = Lazy::new(Default::default);
static COMPLETION_PROMPT: Lazy<Mutex<String>> = Lazy::new(|| {
	Mutex::new(String::from(
		"For the personal conversational transcript above, here is a coaching prompt:",
	))
});

#[derive(Clone)]
struct ChatMessage {
	role: Role,
	content: String,
}

struct WheelWindow(Vec<ChatMessage>);

impl Default for WheelWindow {
	fn default() -> Self {
		Self(vec![ChatMessage { role: User, content: String::new() }])
	}
}

static WHEEL_WINDOWS: Lazy<Mutex<Vec<WheelWindow>>> = Lazy::new(Default::default);

#[derive(Turbosql, Default)]
struct Setting {
	rowid: Option<i64>,
	key: String,
	value: String,
}

impl Setting {
	fn get(key: &str) -> Self {
		select!(Setting "WHERE key = " key)
			.unwrap_or(Setting { key: key.to_string(), ..Default::default() })
	}
	fn save(&self) {
		if self.rowid.is_some() {
			self.update().unwrap();
		} else {
			self.insert().unwrap();
		}
	}
}

#[derive(Turbosql, Default)]
struct SampleData {
	rowid: Option<i64>,
	record_ms: i64,
	sample_data: Blob,
}

#[derive(Turbosql, Default)]
struct Card {
	rowid: Option<i64>,
	deleted: bool,
	title: String,
	question: String,
	answer: String,
	last_question_viewed_ms: i64,
	last_answer_viewed_ms: i64,
}

#[allow(clippy::enum_variant_names)]
#[derive(Serialize, Deserialize, Default)]
enum Action {
	#[default]
	NoAction,
	ViewedQuestion,
	ViewedAnswer,
	Responded {
		correct: bool,
	},
}

#[derive(Turbosql, Default)]
struct CardLog {
	rowid: Option<i64>,
	card_id: i64,
	time_ms: i64,
	action: Action,
}

#[derive(Turbosql, Default)]
struct Prompt {
	rowid: Option<i64>,
	time_ms: i64,
	prompt: String,
}

struct Resource {
	/// HTTP response
	response: ehttp::Response,

	text: Option<String>,

	/// If set, the response was an image.
	image: Option<Image<'static>>,

	/// If set, the response was text with some supported syntax highlighting (e.g. ".rs" or ".md").
	colored_text: Option<ColoredText>,
}

impl Resource {
	fn from_response(ctx: &Context, response: ehttp::Response) -> Self {
		let content_type = response.content_type().unwrap_or_default();
		if content_type.starts_with("image/") {
			ctx.include_bytes(response.url.clone(), response.bytes.clone());
			let image = Image::from_uri(response.url.clone());

			Self { response, text: None, colored_text: None, image: Some(image) }
		} else {
			let text = response.text();
			let colored_text = text.and_then(|text| syntax_highlighting(ctx, &response, text));
			let text = text.map(|text| text.to_owned());

			Self { response, text, colored_text, image: None }
		}
	}
}

#[derive(Default, Deserialize, Serialize)]
pub struct App {
	url: String,
	line_selected: i64,
	title_text: String,
	question_text: String,
	answer_text: String,
	speaker_names: Vec<String>,
	system_text: String,
	prompt_text: String,
	completion_prompt: String,

	#[serde(skip)]
	debounce_tx: Option<Sender<String>>,
	#[serde(skip)]
	gpt_3_trigger: Option<Trigger>,
	#[serde(skip)]
	trigger: Option<Trigger>,
	#[serde(skip)]
	sessions: Vec<session::Session>,
	#[serde(skip)]
	is_recording: bool,
	#[serde(skip)]
	promise: Option<Promise<ehttp::Result<Resource>>>,
}

impl App {
	fn get_transcript(&self) -> String {
		let mut transcript: String = String::new();

		let words = TRANSCRIPT_FINAL.lock().unwrap();

		let lines = words.split(|word| word.is_none());

		for line in lines {
			let mut current_speaker = 100;

			for word in line {
				if let Some(word) = word {
					if word.speaker() != current_speaker {
						current_speaker = word.speaker();
						transcript.push_str(&format!("\n[{}]: ", self.speaker_names[current_speaker as usize]));
					}
					transcript.push_str(&format!("{} ", word.word()));
				}
			}
		}
		transcript.push('\n');
		transcript
	}

	pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
		cc.egui_ctx.set_visuals(egui::style::Visuals::dark());
		cc.egui_ctx.style_mut(|s| s.visuals.override_text_color = Some(Color32::WHITE));

		egui_extras::install_image_loaders(&cc.egui_ctx);

		let (debounce_tx, mut debounce_rx) = mpsc::channel(10);

		let s = Self {
			debounce_tx: Some(debounce_tx),
			sessions: session::Session::calculate_sessions(),
			completion_prompt: COMPLETION_PROMPT.lock().unwrap().clone(),
			..Default::default()
		};

		let ctx_cloned = cc.egui_ctx.clone();

		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_millis(100));
			loop {
				interval.tick().await;
				ctx_cloned.request_repaint();
			}
		});

		let ctx = cc.egui_ctx.clone();

		// Listen for events
		tokio::spawn(async move {
			let duration = Duration::from_millis(300);
			let mut keys_pressed = false;
			let mut string = String::new();
			let mut _trigger = None;

			loop {
				match tokio::time::timeout(duration, debounce_rx.next()).await {
					Ok(Some(s)) => {
						// keyboard activity
						_trigger = None;
						COMPLETION.lock().unwrap().clear();
						string = s;
						keys_pressed = true;
					}
					Ok(None) => {
						eprintln!("Debounce finished");
						break;
					}
					Err(_) => {
						if keys_pressed && !string.is_empty() {
							// eprintln!("{:?} since keyboard activity: {}", duration, &string);
							let (t, tripwire) = Tripwire::new();
							_trigger = Some(t);
							eprintln!("{}", string);
							let string = format!("{} {}", COMPLETION_PROMPT.lock().unwrap(), string);
							let ctx = ctx.clone();
							tokio::spawn(async move {
								COMPLETION.lock().unwrap().clear();
								let ctx = ctx.clone();
								run_openai_completion(tripwire, string, move |content| {
									// eprint!("{}", content);
									COMPLETION.lock().unwrap().push_str(&content);
									ctx.request_repaint();
								})
								.await
								.ok();
							});
							keys_pressed = false;
						}
					}
				}
			}
		});

		// dbg!(&sessions);
		// let session = sessions.first().unwrap();
		// dbg!(session.duration_ms());
		// dbg!(session.samples().len());

		// Load previous app state (if any).
		// Note that you must enable the `persistence` feature for this to work.
		// if let Some(storage) = cc.storage {
		//     return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
		// }

		s
	}
}

trait MyThings {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool;
}

impl MyThings for Ui {
	fn editable(&mut self, text: &mut dyn TextBuffer) -> bool {
		self
			.add(
				// vec2(400.0, 300.0),
				TextEdit::multiline(text)
					.desired_width(f32::INFINITY)
					// .desired_height(f32::INFINITY)
					.font(FontId::new(30.0, FontFamily::Proportional)),
			)
			.changed()
	}
}

impl eframe::App for App {
	fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
		// ctx.input(|i| {
		// 	if i.key_pressed(Key::ArrowDown) {
		// 		self.line_selected =
		// 			select!(i64 "MIN(rowid) FROM card WHERE NOT deleted AND rowid > " self.line_selected)
		// 				.unwrap_or(self.line_selected);
		// 	} else if i.key_pressed(Key::ArrowUp) {
		// 		self.line_selected =
		// 			select!(i64 "MAX(rowid) FROM card WHERE NOT deleted AND rowid < " self.line_selected)
		// 				.unwrap_or(self.line_selected);
		// 	} else if i.key_pressed(Key::Enter) {
		// 		Card::default().insert().unwrap();
		// 	} else if i.key_pressed(Key::Backspace) {
		// 		let _ = update!("card SET deleted = 1 WHERE rowid = " self.line_selected);
		// 		self.line_selected =
		// 			select!(i64 "MIN(rowid) FROM card WHERE NOT deleted AND rowid > " self.line_selected)
		// 				.unwrap_or(0);
		// 	}
		// });

		// let cards = select!(Vec<Card> "WHERE NOT deleted").unwrap();

		let mut do_all = false;

		SidePanel::left("left_panel").show(ctx, |ui| {
			ui.label(option_env!("BUILD_ID").unwrap_or("DEV"));

			// let mut setting = Setting::get("openai_key");
			// ui.label("openai key:");
			// ui
			// 	.add(TextEdit::singleline(&mut setting.value).desired_width(f32::INFINITY))
			// 	.changed()
			// 	.then(|| setting.save());
			// ui.allocate_space(ui.available_size());

			for (i, session) in self.sessions.iter().enumerate() {
				if ui.button(format!("Session {}: {} s", i, session.duration_ms() as f32 / 1000.0)).clicked() {
					let samples = session.samples();

					tokio::spawn(async move {
						// clear transcript
						TRANSCRIPT_FINAL.lock().unwrap().clear();

						// dbg!(session.samples().len());
						audiofile::save_wav_file(samples);

						let deepgram_api_key =
							env::var("DEEPGRAM_API_KEY").expect("DEEPGRAM_API_KEY environmental variable");

						let dg_client = Deepgram::new(&deepgram_api_key);

						let file = File::open("temp_audio.aac").await.unwrap();

						let source = AudioSource::from_buffer_with_mime_type(file, "audio/aac");

						let options = Options::builder()
							.punctuate(false)
							.diarize(true)
							.model(deepgram::transcription::prerecorded::options::Model::CustomId(
								"nova-2-meeting".to_string(),
							))
							.build();

						eprintln!("transcribing...");
						let response: deepgram::transcription::prerecorded::response::Response = dg_client
							.transcription()
							.make_prerecorded_request_builder(source, &options)
							.query(&[("filler_words", "true")])
							.send()
							.await
							.unwrap()
							.json()
							.await
							.unwrap();

						// let response = dg_client.transcription().prerecorded(source, &options).await.unwrap();
						eprintln!("complete.");
						// dbg!(&response);

						let transcript = &response.results.channels[0].alternatives[0].transcript;
						println!("{}", transcript);

						let words = &response.results.channels[0].alternatives[0].words;

						for word in words.iter() {
							TRANSCRIPT_FINAL.lock().unwrap().push(Some(Word::Prerecorded(word.clone())));
						}
					});
					// samples is single-channel f32 little endian
					// make into mp3 file
				}
			}

			while self.speaker_names.len() < 20 {
				self.speaker_names.push(format!("{}", self.speaker_names.len()));
			}

			if ui
				.add_enabled(
					!self.is_recording,
					Button::new(if self.is_recording { "recording..." } else { "record" }),
				)
				.clicked()
			{
				self.is_recording = true;
				self.sessions.push(session::Session { start_ms: now_ms(), end_ms: i64::MAX });
				tokio::spawn(async {
					println!("transcription starting...");

					let dg = Deepgram::new(env::var("DEEPGRAM_API_KEY").unwrap());

					let mut results = dg
						.transcription()
						.stream_request()
						.stream(microphone_as_stream())
						.encoding("linear16".to_string())
						.sample_rate(44100)
						.channels(1)
						.start()
						.await
						.unwrap();

					println!("transcription started");

					while let Some(result) = results.next().await {
						// println!("got: {:?}", result);
						{
							if let Ok(deepgram::transcription::live::StreamResponse::TranscriptResponse {
								duration,
								is_final,
								channel: Channel { mut alternatives, .. },
								..
							}) = result
							{
								if !is_final {
									*DURATION.lock().unwrap() += duration;
								}

								if let Some(deepgram::transcription::live::Alternatives { words, .. }) =
									alternatives.first_mut()
								{
									for word in words.iter() {
										TRANSCRIPT.lock().unwrap().push(Some(word.clone()));
									}
									TRANSCRIPT.lock().unwrap().push(None);

									if is_final {
										for word in words {
											TRANSCRIPT_FINAL.lock().unwrap().push(Some(Word::Live(word.clone())));
										}
										TRANSCRIPT_FINAL.lock().unwrap().push(None);
									}
								}
							}
						}
					}
				});
			}

			if ui.button("import audio file").clicked() {
				tokio::spawn(async {
					let file = rfd::AsyncFileDialog::new().pick_file().await.unwrap().read().await;

					let source = AudioSource::from_buffer_with_mime_type(file, "audio/aac");

					let options = Options::builder()
						.punctuate(false)
						.diarize(true)
						.model(deepgram::transcription::prerecorded::options::Model::CustomId(
							"nova-2-meeting".to_string(),
						))
						.build();

					let deepgram_api_key =
						env::var("DEEPGRAM_API_KEY").expect("DEEPGRAM_API_KEY environmental variable");

					let dg_client = Deepgram::new(&deepgram_api_key);

					eprintln!("transcribing...");
					let mut response = dg_client.transcription().prerecorded(source, &options).await.unwrap();
					eprintln!("complete.");

					TRANSCRIPT_FINAL.lock().unwrap().extend(
						response.results.channels[0].alternatives[0]
							.words
							.drain(..)
							.map(|w| Some(Word::Prerecorded(w))),
					);
				});
			};

			if ui.button("copy transcript to clipboard").clicked() {
				ui.output_mut(|o| o.copied_text = self.get_transcript());
			};

			if ui.button("add window").clicked() {
				WHEEL_WINDOWS.lock().unwrap().push(Default::default());
			};

			if ui.button("do all").clicked() {
				do_all = true;
			};

			ui.label(format!("Duration: {}", *DURATION.lock().unwrap()));

			for name in self.speaker_names.iter_mut() {
				ui.horizontal(|ui| {
					// ui.label(format!("Speaker {}", i + 1));
					ui.add(TextEdit::singleline(name).desired_width(f32::INFINITY)).changed().then(|| {
						// self.speaker_names[i] = name.clone();
					});
				});
			}

			ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
				// let size = [ui.available_width(), ui.spacing().interact_size.y.max(20.0)];
				// for card in cards {
				// 	let i = card.rowid.unwrap();
				// 	let label = SelectableLabel::new(i == self.line_selected, format!("{}: {}", i, card.title));
				// 	if ui.add_sized(size, label).clicked() {
				// 		self.line_selected = i;
				// 	}
				// }

				let words = TRANSCRIPT.lock().unwrap();

				let lines = words.split(|word| word.is_none());

				for line in lines {
					ui.horizontal_wrapped(|ui| {
						for word in line {
							if let Some(word) = word {
								let color = match word.speaker {
									0 => Color32::RED,
									1 => Color32::GREEN,
									2 => Color32::BLUE,
									3 => Color32::YELLOW,
									4 => Color32::LIGHT_GRAY,
									5 => Color32::DARK_RED,
									6 => Color32::DARK_GREEN,
									7 => Color32::BLACK,
									_ => Color32::WHITE,
								};
								ui.label(egui::RichText::new(word.word.clone()).color(color).size(15.0));
							}
						}
					});
				}
			});
		});

		egui::Window::new("").show(ctx, |ui| {
			if ui
				.add(
					TextEdit::multiline(&mut self.completion_prompt)
						.font(FontId::new(20.0, FontFamily::Monospace))
						.desired_width(f32::INFINITY),
				)
				.changed()
			{
				*COMPLETION_PROMPT.lock().unwrap() = self.completion_prompt.clone();
			}
		});

		let len = WHEEL_WINDOWS.lock().unwrap().len();
		for i in 0..len {
			egui::Window::new(format!("wheel {}", i)).show(ctx, |ui| {
				ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
					if ui.button("copy all to clipboard").clicked() {
						let mut text = self.get_transcript();
						text.push_str("\n");

						for entry in WHEEL_WINDOWS.lock().unwrap().get(i).unwrap().0.iter() {
							text.push_str(&format!("[{}]: {}\n", entry.role, entry.content));
						}

						ui.output_mut(|o| o.copied_text = text);
					}
					ui.label("[transcript goes here]");
					let mut wheel_windows = WHEEL_WINDOWS.lock().unwrap();
					let mut do_it = false;
					let mut do_it_j = 9999;
					for (j, entry) in wheel_windows.get_mut(i).unwrap().0.iter_mut().enumerate() {
						let id = Id::new(i * 1000 + j);
						let editor_has_focus = ui.ctx().memory(|m| m.has_focus(id));

						if editor_has_focus && ui.input_mut(|i| i.consume_key(Modifiers::default(), Key::Tab)) {
							entry.content.push_str(COMPLETION.lock().unwrap().as_str().split('\n').next().unwrap());
							COMPLETION.lock().unwrap().clear();
							if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
								let ccursor = egui::text::CCursor::new(entry.content.chars().count());
								state.set_ccursor_range(Some(egui::text::CCursorRange::one(ccursor)));
								state.store(ctx, id);
								// ui.ctx().memory().request_focus(text_edit_id); // give focus back to the `TextEdit`.
							}
						}
						if editor_has_focus
							&& ui
								.input_mut(|i| i.consume_key(Modifiers { command: true, ..Default::default() }, Key::Enter))
						{
							COMPLETION.lock().unwrap().clear();
							do_it = true;
							do_it_j = j;
						}

						ui.horizontal(|ui| {
							ui.radio_value(&mut entry.role, User, "user");
							ui.radio_value(&mut entry.role, System, "system");
							ui.radio_value(&mut entry.role, Assistant, "assistant");

							let mut layouter = |ui: &egui::Ui, string: &str, wrap_width: f32| {
								let mut job = LayoutJob::default();
								let completion = if editor_has_focus {
									let string = COMPLETION.lock().unwrap();
									string.split('\n').next().unwrap().to_owned()
								} else {
									String::new()
								};
								job.append(
									string,
									0.0,
									TextFormat {
										font_id: FontId::new(20.0, FontFamily::Monospace),
										color: Color32::WHITE,
										..Default::default()
									},
								);
								job.append(
									&completion,
									0.0,
									TextFormat {
										font_id: FontId::new(20.0, FontFamily::Monospace),
										color: Color32::DARK_GRAY,
										..Default::default()
									},
								);
								job.wrap.max_width = wrap_width;
								ui.fonts(|f| f.layout_job(job))
							};

							if ui
								.add(
									TextEdit::multiline(&mut entry.content)
										.id(id)
										.lock_focus(true)
										// .font(FontId::new(20.0, FontFamily::Monospace))
										.desired_width(f32::INFINITY)
										.layouter(&mut layouter),
								)
								.changed()
							{
								// eprintln!("{}", entry.content);
								let debounce_tx = self.debounce_tx.clone();
								let entry_content = entry.content.clone();
								tokio::spawn(async move {
									debounce_tx.unwrap().send(entry_content).await.unwrap();
								});
							};
							// if ui.button("remove").clicked() {
							// 	WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().0.remove(j);
							// }
						});
					}
					if do_it {
						let ref mut messages = wheel_windows.get_mut(i).unwrap().0;
						messages.truncate(do_it_j + 1);
						Prompt { rowid: None, time_ms: now_ms(), prompt: messages.last().unwrap().content.clone() }
							.insert()
							.unwrap();
						let orig_messages = messages.clone();
						messages.push(ChatMessage { role: Assistant, content: String::new() });
						messages.push(ChatMessage { role: User, content: String::new() });
						ui.ctx().memory_mut(|m| m.request_focus(Id::new((i * 1000) + messages.len() - 1)));
						let id = messages.len() - 2;
						let transcript = self.get_transcript();
						let ctx_cloned = ctx.clone();
						let (trigger, tripwire) = Tripwire::new();
						self.trigger = Some(trigger);
						tokio::spawn(async move {
							run_openai(GPT_4, tripwire, orig_messages, transcript, move |content| {
								WHEEL_WINDOWS
									.lock()
									.unwrap()
									.get_mut(i)
									.unwrap()
									.0
									.get_mut(id)
									.unwrap()
									.content
									.push_str(content);
								ctx_cloned.request_repaint();
							})
							.await
							.unwrap();
						});
					}
				});
			});
		}

		egui::Window::new("Transcript").show(ctx, |ui| {
			ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
				let words = TRANSCRIPT_FINAL.lock().unwrap();

				let lines = words.split(|word| word.is_none());

				for line in lines {
					let mut current_speaker = 100;

					ui.horizontal_wrapped(|ui| {
						for word in line {
							if let Some(word) = word {
								if word.speaker() != current_speaker {
									current_speaker = word.speaker();
									ui.end_row();
									ui.horizontal_wrapped(|ui| {
										ui.label(
											RichText::new(format!("[{}]: ", self.speaker_names[current_speaker as usize]))
												.size(30.0),
										);
									});
								}
								ui.label(RichText::new(word.word()).size(30.0));
								// ui.add(Label::new(RichText::new(word.word.clone()).color(color).size(30.0)).wrap(true));
							}
						}
					});
				}
			});
			// ui.allocate_space(ui.available_size());
		});
		CentralPanel::default().show(ctx, |_ui| {});
	}
}

fn ui_url(ui: &mut Ui, _frame: &mut eframe::Frame, url: &mut String) -> bool {
	let mut trigger_fetch = false;

	ui.horizontal(|ui| {
		ui.label("URL:");
		trigger_fetch |= ui.add(TextEdit::singleline(url).desired_width(f32::INFINITY)).lost_focus();
	});

	ui.horizontal(|ui| {
		if ui.button("Random image").clicked() {
			let seed = ui.input(|i| i.time);
			let side = 640;
			*url = format!("https://picsum.photos/seed/{seed}/{side}");
			trigger_fetch = true;
		}
	});

	trigger_fetch
}

fn ui_resource(ui: &mut Ui, resource: &Resource) {
	let Resource { response, text, image, colored_text } = resource;

	ui.monospace(format!("url:          {}", response.url));
	ui.monospace(format!("status:       {} ({})", response.status, response.status_text));
	ui.monospace(format!("content-type: {}", response.content_type().unwrap_or_default()));
	ui.monospace(format!("size:         {:.1} kB", response.bytes.len() as f32 / 1000.0));

	ui.separator();

	ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
		CollapsingHeader::new("Response headers").default_open(false).show(ui, |ui| {
			Grid::new("response_headers").spacing(vec2(ui.spacing().item_spacing.x * 2.0, 0.0)).show(
				ui,
				|ui| {
					for header in &response.headers {
						ui.label(&header.0);
						ui.label(&header.1);
						ui.end_row();
					}
				},
			)
		});

		ui.separator();

		if let Some(text) = &text {
			let tooltip = "Click to copy the response body";
			if ui.button("📋").on_hover_text(tooltip).clicked() {
				ui.ctx().copy_text(text.clone());
			}
			ui.separator();
		}

		if let Some(image) = image {
			ui.add(image.clone());
		} else if let Some(colored_text) = colored_text {
			colored_text.ui(ui);
		} else if let Some(text) = &text {
			selectable_text(ui, text);
		} else {
			ui.monospace("[binary]");
		}
	});
}

fn selectable_text(ui: &mut Ui, mut text: &str) {
	ui.add(TextEdit::multiline(&mut text).desired_width(f32::INFINITY).font(TextStyle::Monospace));
}

// ----------------------------------------------------------------------------
// Syntax highlighting:

fn syntax_highlighting(
	ctx: &Context,
	response: &ehttp::Response,
	text: &str,
) -> Option<ColoredText> {
	let extension_and_rest: Vec<&str> = response.url.rsplitn(2, '.').collect();
	let extension = extension_and_rest.first()?;
	let theme = egui_extras::syntax_highlighting::CodeTheme::from_style(&ctx.style());
	Some(ColoredText(egui_extras::syntax_highlighting::highlight(ctx, &theme, text, extension)))
}

struct ColoredText(text::LayoutJob);

impl ColoredText {
	pub fn ui(&self, ui: &mut Ui) {
		if true {
			// Selectable text:
			let mut layouter = |ui: &Ui, _string: &str, wrap_width: f32| {
				let mut layout_job = self.0.clone();
				layout_job.wrap.max_width = wrap_width;
				ui.fonts(|f| f.layout_job(layout_job))
			};

			let mut text = self.0.text.as_str();
			ui.add(
				TextEdit::multiline(&mut text)
					.font(TextStyle::Monospace)
					.desired_width(f32::INFINITY)
					.layouter(&mut layouter),
			);
		} else {
			let mut job = self.0.clone();
			job.wrap.max_width = ui.available_width();
			let galley = ui.fonts(|f| f.layout_job(job));
			let (response, painter) = ui.allocate_painter(galley.size(), Sense::hover());
			painter.add(Shape::galley(response.rect.min, galley, ui.visuals().text_color()));
		}
	}
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

	eprintln!("database at {:?}", turbosql::db_path());

	self_update::self_update().await.ok();
	// let rt = tokio::runtime::Runtime::new().expect("Unable to create Runtime");

	// // Enter the runtime so that `tokio::spawn` is available immediately.
	// let _enter = rt.enter();

	// std::thread::spawn(move || rt.block_on(async {}));

	eframe::run_native(
		"grace",
		eframe::NativeOptions {
			viewport: egui::ViewportBuilder::default()
				.with_inner_size([400.0, 300.0])
				.with_min_inner_size([300.0, 220.0]),
			..Default::default()
		},
		Box::new(|cc| Box::new(App::new(cc))),
	)?;

	Ok(())
}

fn microphone_as_stream() -> Receiver<Result<Bytes, RecvError>> {
	let (sync_tx, sync_rx) = crossbeam::channel::unbounded();
	let (mut async_tx, async_rx) = mpsc::channel(1);

	thread::spawn(move || {
		let host = cpal::default_host();
		let device = host.default_input_device().unwrap();

		let config = device.supported_input_configs().unwrap();
		for config in config {
			dbg!(&config);
		}

		let config = device.default_input_config().unwrap();

		dbg!(&config);

		let num_channels = config.channels() as usize;

		let stream = match config.sample_format() {
			cpal::SampleFormat::F32 => device
				.build_input_stream(
					&config.into(),
					move |data: &[f32], _: &_| {
						let mut bytes =
							BytesMut::with_capacity((data.len() * std::mem::size_of::<i16>()) / num_channels);
						for sample in data.iter().step_by(num_channels) {
							// if *sample > 0.5 {
							// 	dbg!(sample);
							// }
							bytes.put_i16_le(sample.to_sample::<i16>());
						}
						sync_tx.send(bytes.freeze()).ok();
					},
					|_| panic!(),
					None,
				)
				.unwrap(),
			// cpal::SampleFormat::I16 => device
			// 	.build_input_stream(
			// 		&config.into(),
			// 		move |data: &[i16], _: &_| {
			// 			let mut bytes = BytesMut::with_capacity(data.len() * 2);
			// 			for sample in data {
			// 				bytes.put_i16_le(*sample);
			// 			}
			// 			sync_tx.send(bytes.freeze()).unwrap();
			// 		},
			// 		|_| panic!(),
			// 		None,
			// 	)
			// 	.unwrap(),
			// cpal::SampleFormat::U16 => device
			// 	.build_input_stream(
			// 		&config.into(),
			// 		move |data: &[u16], _: &_| {
			// 			let mut bytes = BytesMut::with_capacity(data.len() * 2);
			// 			for sample in data {
			// 				bytes.put_i16_le(sample.to_sample::<i16>());
			// 			}
			// 			sync_tx.send(bytes.freeze()).unwrap();
			// 		},
			// 		|_| panic!(),
			// 		None,
			// 	)
			// 	.unwrap(),
			_ => panic!("unsupported sample format"),
		};

		stream.play().unwrap();

		loop {
			thread::park();
		}
	});

	tokio::spawn(async move {
		let mut buffer = Vec::with_capacity(150000);
		loop {
			let data = sync_rx.recv();
			if let Ok(data) = &data {
				buffer.extend(data.clone().to_vec());
				if buffer.len() > 100000 {
					let moved_buffer = buffer;
					buffer = Vec::with_capacity(150000);
					tokio::task::spawn_blocking(|| {
						SampleData { rowid: None, record_ms: now_ms(), sample_data: moved_buffer }.insert().unwrap();
					});
				}
			}

			async_tx.send(data).await.unwrap();
		}
	});

	async_rx
}

const GPT_3_5: &str = "gpt-3.5-turbo-0125";
const GPT_4: &str = "gpt-4-0125-preview";

pub(crate) async fn run_openai(
	model: &str,
	tripwire: Tripwire,
	messages: Vec<ChatMessage>,
	transcript: String,
	callback: impl Fn(&String) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
	use async_openai::{types::CreateChatCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::new();

	let mut messages = messages
		.into_iter()
		.map(|m| match m.role {
			System => async_openai::types::ChatCompletionRequestSystemMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			User => async_openai::types::ChatCompletionRequestUserMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			Assistant => async_openai::types::ChatCompletionRequestAssistantMessageArgs::default()
				.content(m.content)
				.build()
				.unwrap()
				.into(),
			_ => panic!("invalid role"),
		})
		.collect::<Vec<ChatCompletionRequestMessage>>();

	if !transcript.is_empty() {
		messages.insert(
			0,
			async_openai::types::ChatCompletionRequestUserMessageArgs::default()
				.content(transcript)
				.build()
				.unwrap()
				.into(),
		);
	}

	// dbg!(&messages);

	let request = CreateChatCompletionRequestArgs::default()
		.model(model)
		.max_tokens(4096u16)
		.messages(messages)
		.build()?;

	let mut stream = client.chat().create_stream(request).await?.take_until_if(tripwire);

	while let Some(result) = stream.next().await {
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|chat_choice| {
					if let Some(ref content) = chat_choice.delta.content {
						callback(content);
					}
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
	}

	Ok(())
}

pub(crate) async fn run_openai_completion(
	tripwire: Tripwire,
	prompt: String,
	callback: impl Fn(&String) + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
	use async_openai::{types::CreateCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::new();

	let mut logit_bias: HashMap<String, serde_json::Value> = HashMap::new();

	["198", "271", "1432", "4815", "1980", "382", "720", "627"].iter().for_each(|s| {
		logit_bias
			.insert(s.to_string(), serde_json::Value::Number(serde_json::Number::from_f64(-100.).unwrap()));
	});

	let request = CreateCompletionRequestArgs::default()
		.model("gpt-3.5-turbo-instruct")
		.max_tokens(100u16)
		.logit_bias(logit_bias)
		.prompt(prompt)
		.n(1)
		.stream(true)
		.build()?;

	let mut stream = client.completions().create_stream(request).await?.take_until_if(tripwire);

	while let Some(result) = stream.next().await {
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|c| {
					callback(&c.text);
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
	}

	Ok(())
}
