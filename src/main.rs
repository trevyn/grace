#![forbid(unsafe_code)]
#![allow(unused_imports)]
#![feature(let_chains)]

use bytes::{BufMut, Bytes, BytesMut};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;
use crossbeam::channel::RecvError;
use deepgram::transcription::live::{Alternatives, Channel, Word};
use deepgram::{Deepgram, DeepgramError};
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
use turbosql::{execute, now_ms, select, update, Blob, Turbosql};

static TRANSCRIPT: Lazy<Mutex<Vec<Option<Word>>>> = Lazy::new(Default::default);
static TRANSCRIPT_FINAL: Lazy<Mutex<Vec<Option<Word>>>> = Lazy::new(Default::default);
static DURATION: Lazy<Mutex<f64>> = Lazy::new(Default::default);

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
pub struct HttpApp {
	url: String,
	line_selected: i64,
	title_text: String,
	question_text: String,
	answer_text: String,
	speaker_names: Vec<String>,

	#[serde(skip)]
	promise: Option<Promise<ehttp::Result<Resource>>>,
}

impl HttpApp {
	pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
		cc.egui_ctx.style_mut(|s| s.visuals.override_text_color = Some(Color32::WHITE));

		egui_extras::install_image_loaders(&cc.egui_ctx);

		// Load previous app state (if any).
		// Note that you must enable the `persistence` feature for this to work.
		// if let Some(storage) = cc.storage {
		//     return eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
		// }

		Self::default()
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

impl eframe::App for HttpApp {
	fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
		ctx.request_repaint();

		ctx.input(|i| {
			if i.key_pressed(Key::ArrowDown) {
				self.line_selected =
					select!(i64 "MIN(rowid) FROM card WHERE NOT deleted AND rowid > " self.line_selected)
						.unwrap_or(self.line_selected);
			} else if i.key_pressed(Key::ArrowUp) {
				self.line_selected =
					select!(i64 "MAX(rowid) FROM card WHERE NOT deleted AND rowid < " self.line_selected)
						.unwrap_or(self.line_selected);
			} else if i.key_pressed(Key::Enter) {
				Card::default().insert().unwrap();
			} else if i.key_pressed(Key::Backspace) {
				let _ = update!("card SET deleted = 1 WHERE rowid = " self.line_selected);
				self.line_selected =
					select!(i64 "MIN(rowid) FROM card WHERE NOT deleted AND rowid > " self.line_selected)
						.unwrap_or(0);
			}
		});

		let cards = select!(Vec<Card> "WHERE NOT deleted").unwrap();

		SidePanel::left("left_panel").show(ctx, |ui| {
			let mut setting = Setting::get("openai_key");
			ui.label("openai key:");
			ui
				.add(TextEdit::singleline(&mut setting.value).desired_width(f32::INFINITY))
				.changed()
				.then(|| setting.save());
			// ui.allocate_space(ui.available_size());

			while self.speaker_names.len() < 20 {
				self.speaker_names.push(format!("{}", self.speaker_names.len()));
			}

			if ui.button("dump").clicked() {
				let words = TRANSCRIPT_FINAL.lock().unwrap();

				let lines = words.split(|word| word.is_none());

				for line in lines {
					let mut current_speaker = 100;

					for word in line {
						if let Some(word) = word {
							if word.speaker != current_speaker {
								current_speaker = word.speaker;
								print!("\n[{}]: ", self.speaker_names[current_speaker as usize]);
							}
							print!("{} ", word.word);
						}
					}
				}
				println!("");
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
				let size = [ui.available_width(), ui.spacing().interact_size.y.max(20.0)];
				for card in cards {
					let i = card.rowid.unwrap();
					let label = SelectableLabel::new(i == self.line_selected, format!("{}: {}", i, card.title));
					if ui.add_sized(size, label).clicked() {
						self.line_selected = i;
					}
				}

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

		// SidePanel::right("right_panel").show(ctx, |ui| {});

		CentralPanel::default().show(ctx, |ui| {
			ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
				let words = TRANSCRIPT_FINAL.lock().unwrap();

				let lines = words.split(|word| word.is_none());

				for line in lines {
					let mut current_speaker = 100;

					ui.horizontal_wrapped(|ui| {
						for word in line {
							if let Some(word) = word {
								if word.speaker != current_speaker {
									current_speaker = word.speaker;
									ui.end_row();
									ui.horizontal_wrapped(|ui| {
										ui.label(
											RichText::new(format!("[{}]: ", self.speaker_names[current_speaker as usize]))
												.size(30.0),
										);
									});
								}
								ui.label(RichText::new(word.word.clone()).size(30.0));
								// ui.add(Label::new(RichText::new(word.word.clone()).color(color).size(30.0)).wrap(true));
							}
						}
					});
				}
			});
		});
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

fn main() -> eframe::Result<()> {
	env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

	let dg = Deepgram::new(env::var("DEEPGRAM_API_KEY").unwrap());

	let rt = tokio::runtime::Runtime::new().expect("Unable to create Runtime");

	// Enter the runtime so that `tokio::spawn` is available immediately.
	let _enter = rt.enter();

	std::thread::spawn(move || {
		rt.block_on(async {
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

			while let Some(result) = results.next().await {
				println!("got: {:?}", result);
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
									TRANSCRIPT_FINAL.lock().unwrap().push(Some(word.clone()));
								}
								TRANSCRIPT_FINAL.lock().unwrap().push(None);
							}
						}
					}
				}
			}
		})
	});

	eframe::run_native(
		"scarlett",
		eframe::NativeOptions {
			viewport: egui::ViewportBuilder::default()
				.with_inner_size([400.0, 300.0])
				.with_min_inner_size([300.0, 220.0]),
			..Default::default()
		},
		Box::new(|cc| Box::new(HttpApp::new(cc))),
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
						SampleData { rowid: None, record_ms: now_ms(), sample_data: bytes.to_vec() }
							.insert()
							.unwrap();
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
		loop {
			let data = sync_rx.recv();
			async_tx.send(data).await.unwrap();
		}
	});

	async_rx
}
