#![forbid(unsafe_code)]
#![allow(unused_imports)]

use egui::*;
use poll_promise::Promise;
use serde::{Deserialize, Serialize};
use turbosql::{execute, select, update, Turbosql};

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
		self.add(TextEdit::multiline(text).font(FontId::new(30.0, FontFamily::Proportional))).changed()
	}
}

impl eframe::App for HttpApp {
	fn update(&mut self, ctx: &Context, frame: &mut eframe::Frame) {
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
			ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
				let size = [ui.available_width(), ui.spacing().interact_size.y.max(20.0)];
				for card in cards {
					let i = card.rowid.unwrap();
					let label = SelectableLabel::new(i == self.line_selected, format!("{}: {}", i, card.title));
					if ui.add_sized(size, label).clicked() {
						self.line_selected = i;
					}
				}
			});
		});

		SidePanel::right("right_panel").show(ctx, |ui| {
			let mut setting = Setting::get("openai_key");
			ui.label("openai key:");
			ui
				.add(TextEdit::singleline(&mut setting.value).desired_width(f32::INFINITY))
				.changed()
				.then(|| setting.save());
			ui.allocate_space(ui.available_size());
		});

		CentralPanel::default().show(ctx, |ui| {
			let prev_url = self.url.clone();
			let trigger_fetch = ui_url(ui, frame, &mut self.url);

			if trigger_fetch {
				let ctx = ctx.clone();
				let (sender, promise) = Promise::new();
				let request = ehttp::Request::get(&self.url);
				ehttp::fetch(request, move |response| {
					ctx.forget_image(&prev_url);
					ctx.request_repaint(); // wake up UI thread
					let resource = response.map(|response| Resource::from_response(&ctx, response));
					sender.send(resource);
				});
				self.promise = Some(promise);
			}

			if let Ok(mut card) = select!(Card "WHERE rowid = " self.line_selected) {
				ui.label(format!("Card number: {}", card.rowid.unwrap()));
				ui.label("title:");
				ui.editable(&mut card.title).then(|| card.update());
				ui.label("question:");
				ui.editable(&mut card.question).then(|| card.update());
				ui.label("answer:");
				ui.editable(&mut card.answer).then(|| card.update());
				ui.separator();
			}

			if let Some(promise) = &self.promise {
				if let Some(result) = promise.ready() {
					match result {
						Ok(resource) => {
							ui_resource(ui, resource);
						}
						Err(error) => {
							// This should only happen if the fetch API isn't available or something similar.
							ui
								.colored_label(ui.visuals().error_fg_color, if error.is_empty() { "Error" } else { error });
						}
					}
				} else {
					ui.spinner();
				}
			}
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

	ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
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
