#![forbid(unsafe_code)]
#![allow(unused_imports)]
// #![feature(let_chains)]

use async_openai::types::ChatCompletionRequestSystemMessageArgs;
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
use egui::*;
use futures::channel::mpsc::{self, Receiver as FuturesReceiver};
use futures::stream::StreamExt;
use futures::SinkExt;
use once_cell::sync::Lazy;
use poll_promise::Promise;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Mutex;
use std::thread;
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

#[derive(Default)]
struct WheelWindow {
	system: String,
	prompt: String,
	completion: String,
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
		cc.egui_ctx.style_mut(|s| s.visuals.override_text_color = Some(Color32::WHITE));

		egui_extras::install_image_loaders(&cc.egui_ctx);

		let mut s = Self::default();

		s.sessions = session::Session::calculate_sessions();
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
		ctx.request_repaint();

		// self.sessions = session::Session::calculate_sessions();

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
						let response = dg_client.transcription().prerecorded(source, &options).await.unwrap();
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

			if ui.button("dump").clicked() {
				println!("{}", self.get_transcript());
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

		let len = WHEEL_WINDOWS.lock().unwrap().len();
		for i in 0..len {
			egui::Window::new(format!("wheel {}", i)).show(ctx, |ui| {
				ui.add(
					TextEdit::multiline(&mut WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().system)
						.font(FontId::new(20.0, FontFamily::Monospace))
						.desired_width(f32::INFINITY),
				);
				ui.label("[transcript goes here]");
				ui.add(
					TextEdit::multiline(&mut WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().prompt)
						.font(FontId::new(20.0, FontFamily::Monospace))
						.desired_width(f32::INFINITY),
				);
				if do_all || ui.button("do it").clicked() {
					{
						WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().completion.clear();
					}
					// let system = wheelwindow.system.clone();
					// let prompt = wheelwindow.prompt.clone();
					let transcript = self.get_transcript();
					tokio::spawn(async move {
						run_openai(i, transcript).await.unwrap();
					});
				};
				// ui.code_editor(&mut self.editor_text).font_size(0.0);
				ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
					ui.label(
						egui::RichText::new(WHEEL_WINDOWS.lock().unwrap().get(i).unwrap().completion.as_str())
							.font(FontId::new(20.0, FontFamily::Monospace)),
					);
				});
				// 	Label::new(OPENAI.lock().unwrap().as_str()).size(FontId::new(20.0, FontFamily::Monospace)),
				// );
			});
		}

		// egui::Window::new("wheel 1").show(ctx, |ui| {
		// 	ui.add(
		// 		TextEdit::multiline(&mut self.system_text)
		// 			.font(FontId::new(20.0, FontFamily::Monospace))
		// 			.desired_width(f32::INFINITY),
		// 	);
		// 	ui.label("[transcript goes here]");
		// 	ui.add(
		// 		TextEdit::multiline(&mut self.prompt_text)
		// 			.font(FontId::new(20.0, FontFamily::Monospace))
		// 			.desired_width(f32::INFINITY),
		// 	);
		// 	if ui.button("do it").clicked() {
		// 		OPENAI.lock().unwrap().clear();
		// 		let system = self.system_text.clone();
		// 		let prompt = self.prompt_text.clone();
		// 		let transcript = self.get_transcript();
		// 		tokio::spawn(async {
		// 			run_openai(system, transcript, prompt).await.unwrap();
		// 		});
		// 	};
		// 	// ui.code_editor(&mut self.editor_text).font_size(0.0);
		// 	ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
		// 		ui.label(
		// 			egui::RichText::new(OPENAI.lock().unwrap().as_str())
		// 				.font(FontId::new(20.0, FontFamily::Monospace)),
		// 		);
		// 	});
		// 	// 	Label::new(OPENAI.lock().unwrap().as_str()).size(FontId::new(20.0, FontFamily::Monospace)),
		// 	// );
		// });
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
		CentralPanel::default().show(ctx, |ui| {});
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
async fn main() -> eframe::Result<()> {
	env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

	self_update::self_update().await.ok();
	// let rt = tokio::runtime::Runtime::new().expect("Unable to create Runtime");

	// // Enter the runtime so that `tokio::spawn` is available immediately.
	// let _enter = rt.enter();

	// std::thread::spawn(move || rt.block_on(async {}));

	eframe::run_native(
		"scarlett",
		eframe::NativeOptions {
			viewport: egui::ViewportBuilder::default()
				.with_inner_size([400.0, 300.0])
				.with_min_inner_size([300.0, 220.0]),
			..Default::default()
		},
		Box::new(|cc| Box::new(App::new(cc))),
	)
}

fn microphone_as_stream() -> FuturesReceiver<Result<Bytes, RecvError>> {
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

		let stream = match config.sample_format() {
			cpal::SampleFormat::F32 => device
				.build_input_stream(
					&config.into(),
					move |data: &[f32], _: &_| {
						// dbg!(data.len());
						let mut bytes = BytesMut::with_capacity(data.len() * 4);
						for sample in data {
							bytes.put_f32_le(*sample);
						}
						let mut bytes = BytesMut::with_capacity(data.len() * 2);
						for sample in data {
							// if *sample > 0.5 {
							// 	dbg!(sample);
							// }
							bytes.put_i16_le(sample.to_sample::<i16>());
						}
						sync_tx.send(bytes.freeze()).unwrap();
					},
					|_| panic!(),
					None,
				)
				.unwrap(),
			cpal::SampleFormat::I16 => device
				.build_input_stream(
					&config.into(),
					move |data: &[i16], _: &_| {
						let mut bytes = BytesMut::with_capacity(data.len() * 2);
						for sample in data {
							bytes.put_i16_le(*sample);
						}
						sync_tx.send(bytes.freeze()).unwrap();
					},
					|_| panic!(),
					None,
				)
				.unwrap(),
			cpal::SampleFormat::U16 => device
				.build_input_stream(
					&config.into(),
					move |data: &[u16], _: &_| {
						let mut bytes = BytesMut::with_capacity(data.len() * 2);
						for sample in data {
							bytes.put_i16_le(sample.to_sample::<i16>());
						}
						sync_tx.send(bytes.freeze()).unwrap();
					},
					|_| panic!(),
					None,
				)
				.unwrap(),
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

pub(crate) async fn run_openai(
	i: usize,
	transcript: String,
) -> Result<(), Box<dyn std::error::Error>> {
	use std::io::{stdout, Write};

	use async_openai::types::ChatCompletionRequestUserMessageArgs;
	use async_openai::{types::CreateChatCompletionRequestArgs, Client};
	use futures::StreamExt;

	let client = Client::new();

	let system = WHEEL_WINDOWS.lock().unwrap().get(i).unwrap().system.clone();
	let prompt = WHEEL_WINDOWS.lock().unwrap().get(i).unwrap().prompt.clone();

	let request = CreateChatCompletionRequestArgs::default()
		.model("gpt-4-0125-preview")
		// .model("gpt-3.5-turbo-0125")
		.max_tokens(4096u16)
		.messages([
			ChatCompletionRequestSystemMessageArgs::default().content(system).build()?.into(),
			ChatCompletionRequestUserMessageArgs::default().content(transcript).build()?.into(),
			ChatCompletionRequestUserMessageArgs::default().content(prompt).build()?.into(),
		])
		.build()?;

	let mut stream = client.chat().create_stream(request).await?;

	while let Some(result) = stream.next().await {
		match result {
			Ok(response) => {
				response.choices.iter().for_each(|chat_choice| {
					if let Some(ref content) = chat_choice.delta.content {
						// print!("{}", content);
						WHEEL_WINDOWS.lock().unwrap().get_mut(i).unwrap().completion.push_str(content);
						// OPENAI.lock().unwrap().push_str(content);
					}
				});
			}
			Err(err) => {
				panic!("error: {err}");
			}
		}
		// stdout().flush()?;
	}

	Ok(())
}
