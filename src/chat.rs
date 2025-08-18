#[cfg(feature = "tts")]
use crate::sessions::SharedTts;

use crate::{
    easymark::MemoizedEasymarkHighlighter,
    file_handler::convert_file_to_part,
    widgets::{self, GeminiModel, ModelPicker, Settings},
};
use anyhow::{Context, Result};
use eframe::{
    egui::{
        self, pos2, vec2, Align, Color32, CornerRadius, Frame, Key, KeyboardShortcut, Layout,
        Margin, Modifiers, Pos2, Rect, Stroke, TextStyle,
    },
    epaint::text,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use egui_modal::{Icon, Modal};
use egui_virtual_list::VirtualList;
use flowync::{error::Compact, CompactFlower, CompactHandle};
use gemini_client_api::gemini::{
    ask::Gemini,
    types::{
        request::{Part, SystemInstruction},
        response::GeminiResponseStream,
        sessions::Session,
    },
};
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio_stream::StreamExt;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Message {
    model: GeminiModel,
    content: String,
    role: Role,
    #[serde(skip)]
    is_generating: bool,
    #[serde(skip)]
    requested_at: Instant,
    time: chrono::DateTime<chrono::Utc>,
    #[serde(skip)]
    clicked_copy: bool,
    is_error: bool,
    #[serde(skip)]
    is_speaking: bool,
    files: Vec<PathBuf>,
    is_prepending: bool,
    is_thought: bool,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            content: String::new(),
            role: Role::User,
            is_generating: false,
            requested_at: Instant::now(),
            time: chrono::Utc::now(),
            clicked_copy: false,
            is_error: false,
            is_speaking: false,
            model: GeminiModel::default(),
            files: Vec::new(),
            is_prepending: false,
            is_thought: false,
        }
    }
}

#[cfg(feature = "tts")]
fn tts_control(tts: SharedTts, text: String, speak: bool) {
    std::thread::spawn(move || {
        if let Some(tts) = tts {
            if speak {
                let _ = tts
                    .write()
                    .speak(widgets::sanitize_text_for_tts(&text), true)
                    .map_err(|e| log::error!("failed to speak: {e}"));
            } else {
                let _ = tts
                    .write()
                    .stop()
                    .map_err(|e| log::error!("failed to stop tts: {e}"));
            }
        }
    });
}

fn make_short_name(name: &str) -> String {
    // todo lmao
    // let mut c = name
    //     .split('/')
    //     .next()
    //     .unwrap_or(name)
    //     .chars()
    //     .take_while(|c| c.is_alphanumeric());
    // match c.next() {
    //     None => "Gemini".to_string(),
    //     Some(f) => f.to_uppercase().collect::<String>() + c.collect::<String>().as_str(),
    // }
    "Gemini".to_string()
}

enum MessageAction {
    None,
    Retry(usize),
    Regenerate(usize),
}

impl Message {
    #[inline]
    fn user(content: String, model: GeminiModel, files: Vec<PathBuf>) -> Self {
        Self {
            content,
            role: Role::User,
            is_generating: false,
            model,
            files,
            ..Default::default()
        }
    }

    #[inline]
    fn assistant(content: String, model: GeminiModel) -> Self {
        Self {
            content,
            role: Role::Assistant,
            is_generating: true,
            model,
            ..Default::default()
        }
    }

    #[inline]
    const fn is_user(&self) -> bool {
        matches!(self.role, Role::User)
    }

    fn show(
        &mut self,
        ui: &mut egui::Ui,
        commonmark_cache: &mut CommonMarkCache,
        #[cfg(feature = "tts")] tts: SharedTts,
        idx: usize,
        prepend_buf: &mut String,
    ) -> MessageAction {
        // message role
        let message_offset = ui
            .horizontal(|ui| {
                if self.is_user() {
                    let f = ui.label("👤").rect.left();
                    ui.label("You").rect.left() - f
                } else {
                    let f = ui.label("✨").rect.left();
                    let offset = ui
                        .label(make_short_name(&self.model.to_string()))
                        .on_hover_text(&self.model.to_string())
                        .rect
                        .left()
                        - f;
                    ui.add_enabled(false, egui::Label::new(&self.model.to_string()));
                    offset
                }
            })
            .inner;

        if self.is_thought {
            ui.horizontal(|ui| {
                ui.add_space(message_offset);
                let done_thinking = !self.is_generating;
                Frame::group(ui.style())
                    .inner_margin(Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        // egui::collapsing_header::CollapsingState::load_with_default_open
                        egui::CollapsingHeader::new("  Thoughts")
                            .id_salt(self.time.timestamp_millis())
                            .default_open(false)
                            .icon(move |ui, openness, response| {
                                widgets::thinking_icon(ui, openness, response, done_thinking);
                            })
                            .show(ui, |ui| {
                                CommonMarkViewer::new().show(ui, commonmark_cache, &self.content);
                            });
                    });
            });
            ui.add_space(4.0);
            return MessageAction::None;
        }

        let is_commonmark = !self.content.is_empty() && !self.is_error && !self.is_prepending;
        if is_commonmark {
            ui.add_space(-TextStyle::Body.resolve(ui.style()).size + 4.0);
        }

        // message content / spinner
        let mut action = MessageAction::None;
        ui.horizontal(|ui| {
            ui.add_space(message_offset);
            if self.content.is_empty() && self.is_generating && !self.is_error {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());

                    // show time spent waiting for response
                    ui.add_enabled(
                        false,
                        egui::Label::new(format!(
                            "{:.1}s",
                            self.requested_at.elapsed().as_secs_f64()
                        )),
                    )
                });
            } else if self.is_error {
                ui.label(self.content.clone());
                if ui
                    .button("Retry")
                    .on_hover_text(
                        "Try to generate a response again. Make sure you have a valid API Key.",
                    )
                    .clicked()
                {
                    action = MessageAction::Retry(idx);
                }
            } else if self.is_prepending {
                let textedit = ui.add(
                    egui::TextEdit::multiline(prepend_buf).hint_text("Prepend text to response…"),
                );
                macro_rules! cancel_prepend {
                    () => {
                        self.is_prepending = false;
                        prepend_buf.clear();
                    };
                }
                if textedit.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel_prepend!();
                }
                ui.vertical(|ui| {
                    if ui
                        .button("🔄 Regenerate")
                        .on_hover_text(
                            "Generate the response again, \
                            the LLM will start after any prepended text",
                        )
                        .clicked()
                    {
                        self.content = prepend_buf.clone();
                        self.is_prepending = false;
                        self.is_generating = true;
                        action = MessageAction::Regenerate(idx);
                    }
                    if !prepend_buf.is_empty()
                        && ui
                            .button("\u{270f} Edit")
                            .on_hover_text(
                                "Edit the message in the context, but don't regenerate it",
                            )
                            .clicked()
                    {
                        self.content = prepend_buf.clone();
                        cancel_prepend!();
                    }
                    if ui.button("❌ Cancel").clicked() {
                        cancel_prepend!();
                    }
                });
            } else {
                CommonMarkViewer::new().max_image_width(Some(512)).show(
                    ui,
                    commonmark_cache,
                    &self.content,
                );
            }
        });

        // files
        if !self.files.is_empty() {
            if is_commonmark {
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.add_space(message_offset);
                egui::ScrollArea::horizontal().id_salt(idx).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        crate::file_handler::show_files(ui, &mut self.files, false);
                    });
                })
            });
            ui.add_space(8.0);
        }

        if self.is_prepending {
            return action;
        }

        // copy buttons and such
        let shift_held = !ui.ctx().wants_keyboard_input() && ui.input(|i| i.modifiers.shift);

        if !self.is_generating && !self.is_error {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(message_offset);
                if !self.content.is_empty() {
                    let copy = ui
                        .add(
                            egui::Button::new(if self.clicked_copy { "✔" } else { "🗐" })
                                .small()
                                .fill(egui::Color32::TRANSPARENT),
                        )
                        .on_hover_text(if self.clicked_copy {
                            "Copied!"
                        } else {
                            "Copy message"
                        });
                    if copy.clicked() {
                        ui.ctx().copy_text(self.content.clone());
                        self.clicked_copy = true;
                    }
                    self.clicked_copy = self.clicked_copy && copy.hovered();
                }

                #[cfg(feature = "tts")]
                {
                    let speak = ui
                        .add(
                            egui::Button::new(if self.is_speaking { "…" } else { "🔊" })
                                .small()
                                .fill(egui::Color32::TRANSPARENT),
                        )
                        .on_hover_text("Read the message out loud. Right click to repeat");

                    if speak.clicked() {
                        if self.is_speaking {
                            self.is_speaking = false;
                            tts_control(tts, String::new(), false);
                        } else {
                            self.is_speaking = true;
                            tts_control(tts, self.content.clone(), true);
                        }
                    } else if speak.secondary_clicked() {
                        self.is_speaking = true;
                        tts_control(tts, self.content.clone(), true);
                    }
                }

                if ui
                    .add(
                        egui::Button::new("🗑")
                            .small()
                            .fill(egui::Color32::TRANSPARENT),
                    )
                    .on_hover_text("Remove")
                    .clicked()
                {
                    dbg!("not implemented yer!");
                }

                if !self.is_user()
                    && prepend_buf.is_empty()
                    && ui
                        .add(
                            egui::Button::new("🔄")
                                .small()
                                .fill(egui::Color32::TRANSPARENT),
                        )
                        .on_hover_text("Regenerate")
                        .clicked()
                {
                    prepend_buf.clear();
                    self.is_prepending = true;
                }
            });
        }
        ui.add_space(12.0);

        action
    }
}

// <completion progress, final completion, error>
type CompletionFlower = CompactFlower<(usize, Part), (usize, String), (usize, String)>;
type CompletionFlowerHandle = CompactHandle<(usize, Part), (usize, String), (usize, String)>;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Chat {
    chatbox: String,
    pub messages: Vec<Message>,
    pub summary: String,
    stop_generating: Arc<AtomicBool>,
    pub model_picker: ModelPicker,
    pub files: Vec<PathBuf>,
    prepend_buf: String,

    #[serde(skip)]
    chatbox_height: f32,
    #[serde(skip)]
    flower: CompletionFlower,
    #[serde(skip)]
    retry_message_idx: Option<usize>,
    #[serde(skip)]
    virtual_list: VirtualList,
    #[serde(skip)]
    chatbox_highlighter: MemoizedEasymarkHighlighter,
}

impl Default for Chat {
    fn default() -> Self {
        Self {
            chatbox: String::new(),
            chatbox_height: 0.0,
            messages: Vec::new(),
            flower: CompletionFlower::new(1),
            retry_message_idx: None,
            summary: String::new(),
            chatbox_highlighter: MemoizedEasymarkHighlighter::default(),
            stop_generating: Arc::new(AtomicBool::new(false)),
            virtual_list: {
                let mut list = VirtualList::new();
                list.check_for_resize(false);
                list
            },
            model_picker: ModelPicker::default(),
            files: Vec::new(),
            prepend_buf: String::new(),
        }
    }
}

async fn request_completion(
    mut gemini: Gemini,
    messages: Vec<Message>,
    handle: &CompletionFlowerHandle,
    stop_generating: Arc<AtomicBool>,
    index: usize,
    use_streaming: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::info!(
        "requesting completion... (history length: {})",
        messages.len()
    );

    // Build a gemini-client-api session from the message history
    let mut gemini_session = Session::new(messages.len());

    // Regenerate from a certain point if needed
    let messages_to_process = if messages.get(index).map_or(false, |m| m.is_generating) {
        &messages[..index]
    } else {
        &messages
    };

    // A buffer to hold parts for the current consecutive group of messages.
    let mut parts_buffer = Vec::new();
    // Tracks the author of the current group. `None` means we're at the start.
    let mut current_author_is_user: Option<bool> = None;

    for message in messages_to_process {
        // Skip messages that should not be part of the conversation history.
        if message.is_thought || (message.content.is_empty() && message.files.is_empty()) {
            continue;
        }

        let message_author_is_user = message.is_user();

        // Check if the author has changed from the previous message.
        // `current_author.is_some()` handles the very first message.
        if current_author_is_user.is_some()
            && current_author_is_user != Some(message_author_is_user)
        {
            // Author has changed. The previous group is complete. Submit it.
            // Use `std::mem::take` to efficiently swap the buffer with an empty Vec.
            let completed_parts = std::mem::take(&mut parts_buffer);
            if !completed_parts.is_empty() {
                if current_author_is_user.unwrap() {
                    // unwrap is safe here
                    gemini_session.ask(completed_parts);
                } else {
                    gemini_session.reply(completed_parts);
                }
            }
        }

        // -- Process the current message and add its parts to the buffer --

        // Update the author for the current (or new) group.
        current_author_is_user = Some(message_author_is_user);

        for file_path in &message.files {
            match convert_file_to_part(file_path).await {
                Ok(part) => {
                    parts_buffer.push(Part::text(
                        format!("File with name: {}", file_path.file_name().unwrap_or_default().to_string_lossy()).into()
                    ));
                    parts_buffer.push(part)
                },
                Err(e) => log::error!("Failed to convert file {}: {}", file_path.display(), e), // todo say to ui
            }
        }

        if !message.content.is_empty() {
            parts_buffer.push(Part::text(message.content.clone().into()));
        }
    }

    // After the loop, the last group of messages might still be in the buffer.
    // We need to submit this final batch.
    if !parts_buffer.is_empty() {
        if let Some(is_user) = current_author_is_user {
            if is_user {
                gemini_session.ask(parts_buffer);
            } else {
                gemini_session.reply(parts_buffer);
            }
        }
    }

    dbg!(&gemini_session);

    // Handle the prepended text for regeneration // TODO BROKEN!
    if let Some(msg) = messages.get(index) {
        if !msg.content.is_empty() {
            gemini_session.reply(vec![Part::text(msg.content.clone().into())]);
        }
    }

    let mut response_text = String::new();
    if use_streaming {
        let mut stream = gemini
            .ask_as_stream(gemini_session)
            .await
            .map_err(|err| err.1)?;

        log::info!("reading response...");
        while let Some(Ok(res)) = stream.next().await {
            if stop_generating.load(Ordering::SeqCst) {
                log::info!("stopping generation");
                drop(stream);
                stop_generating.store(false, Ordering::SeqCst);
                break;
            }

            for part in res.get_parts() {
                handle.send((index, part.clone()));
                match part {
                    Part::text(info) => {
                        response_text += info.text();
                    }
                    _ => {}
                }
            }
        }
    } else {
        let cancellation_checker = async {
            loop {
                if stop_generating.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        };

        log::info!("sending non-streaming request...");
        tokio::select! {  // todo some working bullshit
            biased;

            _ = cancellation_checker => {
                log::info!("non-streaming generation cancelled by user.");
                stop_generating.store(false, Ordering::SeqCst);
            }

            result = gemini.ask(&mut gemini_session) => {
                match result {
                    Ok(response) => {
                        log::info!("reading non-streamed response...");
                        let mut response_text = String::new();
                        for part in response.get_parts() {
                            handle.send((index, part.clone()));
                            if let Part::text(info) = part {
                                response_text += info.text();
                            }
                        }
                        log::info!(
                            "non-streaming completion request complete, response length: {}",
                            response_text.len()
                        );
                        handle.success((index, response_text));
                        return Ok(());
                    }
                    Err(err) => return Err(err)?,
                }
            }
        }
    }

    log::info!(
        "completion request complete, response length: {}",
        response_text.len()
    );
    handle.success((index, response_text));
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy, serde::Deserialize, serde::Serialize)]
pub enum ChatExportFormat {
    #[default]
    Plaintext,
    Json,
    Ron,
}

impl std::fmt::Display for ChatExportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl ChatExportFormat {
    pub const ALL: [Self; 3] = [Self::Plaintext, Self::Json, Self::Ron];

    #[inline]
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Plaintext => &["txt"],
            Self::Json => &["json"],
            Self::Ron => &["ron"],
        }
    }
}

pub async fn export_messages(
    messages: Vec<Message>,
    format: ChatExportFormat,
    task: impl std::future::Future<Output = Option<rfd::FileHandle>>,
) -> Result<egui_notify::Toast> {
    let Some(file) = task.await else {
        log::info!("export cancelled");
        return Ok(egui_notify::Toast::info("Export cancelled"));
    };
    log::info!(
        "exporting {} messages to {file:?} (format: {format:?})...",
        messages.len()
    );

    let f = std::fs::File::create(file.path())?;
    let mut f = std::io::BufWriter::new(f);

    match format {
        ChatExportFormat::Plaintext => {
            for msg in &messages {
                writeln!(
                    f,
                    "{} - {:?} ({}): {}",
                    msg.time.to_rfc3339(),
                    msg.role,
                    msg.model,
                    msg.content
                )?;
            }
        }
        ChatExportFormat::Json => {
            serde_json::to_writer_pretty(&mut f, &messages)?;
        }
        ChatExportFormat::Ron => {
            ron::Options::default().to_io_writer_pretty(&mut f, &messages, Default::default())?;
        }
    }

    f.flush().context("failed to flush writer")?;

    log::info!("export complete");
    Ok(egui_notify::Toast::success(format!(
        "Exported {} messages to {}",
        messages.len(),
        file.file_name(),
    )))
}

fn make_summary(prompt: &str) -> String {
    const MAX_SUMMARY_LENGTH: usize = 24;
    let mut summary = String::with_capacity(MAX_SUMMARY_LENGTH);
    for (i, ch) in prompt.chars().enumerate() {
        if i >= MAX_SUMMARY_LENGTH {
            summary.push('…');
            break;
        }
        if ch == '\n' {
            break;
        }
        if i == 0 {
            summary += &ch.to_uppercase().to_string();
        } else {
            summary.push(ch);
        }
    }
    summary
}

#[derive(Debug, Clone, Copy)]
pub enum ChatAction {
    None,
    PickFiles { id: usize },
}

impl Chat {
    #[inline]
    pub fn new(id: usize, model_picker: ModelPicker) -> Self {
        Self {
            flower: CompletionFlower::new(id),
            model_picker,
            ..Default::default()
        }
    }

    #[inline]
    pub fn id(&self) -> usize {
        self.flower.id()
    }

    fn send_message(&mut self, settings: &Settings) {
        if self.chatbox.is_empty() && self.files.is_empty() {
            return;
        }

        // remove old error messages
        self.messages.retain(|m| !m.is_error);

        let prompt = self.chatbox.trim_end().to_string();
        let model = self.model_picker.selected;
        self.messages
            .push(Message::user(prompt.clone(), model, self.files.clone()));

        if self.summary.is_empty() {
            self.summary = make_summary(&prompt);
        }

        self.chatbox.clear();
        self.files.clear();

        self.messages.push(Message::assistant(String::new(), model));

        self.spawn_completion(settings);
    }

    fn spawn_completion(&self, settings: &Settings) {
        let handle = self.flower.handle();
        let stop_generation = self.stop_generating.clone();
        let mut messages = self.messages.clone();
        let index = self.messages.len() - 1;

        if settings.include_thoughts_in_history {
            for msg in &mut messages {
                if msg.is_thought {
                    msg.is_thought = false;
                    msg.content.insert_str(0, "MY INNER REFLECTIONS: ");
                    msg.content
                        .push_str(r"--- end of inner reflections ---\r\n")
                }
            }
        }

        let no_api_key = settings.api_key.is_empty();
        let use_streaming = settings.use_streaming;

        let gemini = self
            .model_picker
            .create_client(&settings.api_key, settings.proxy_path.clone());

        tokio::spawn(async move {
            handle.activate();

            if no_api_key {
                handle.error((index, "API key not set.".to_string()));
                return;
            }

            let _ = request_completion(
                gemini,
                messages,
                &handle,
                stop_generation,
                index,
                use_streaming,
            )
            .await
            .map_err(|e| {
                log::error!("failed to request completion: {e}");
                handle.error((index, e.to_string()));
            });
        });
    }

    fn regenerate_response(&mut self, settings: &Settings, idx: usize) {
        // todo: regenerate works weird
        self.messages[idx].content = self.prepend_buf.clone();
        self.prepend_buf.clear();

        self.spawn_completion(settings);
    }

    fn show_chatbox(
        &mut self,
        ui: &mut egui::Ui,
        is_max_height: bool,
        is_generating: bool,
        settings: &Settings,
    ) -> ChatAction {
        let mut action = ChatAction::None;
        if let Some(idx) = self.retry_message_idx.take() {
            self.chatbox = self.messages[idx - 1].content.clone();
            self.files = self.messages[idx - 1].files.clone();
            self.messages.remove(idx);
            self.messages.remove(idx - 1);
            self.send_message(settings);
        }

        if is_max_height {
            ui.add_space(8.0);
        }

        let images_height = if !self.files.is_empty() {
            ui.add_space(8.0);
            let height = ui
                .horizontal(|ui| {
                    crate::file_handler::show_files(ui, &mut self.files, true);
                })
                .response
                .rect
                .height();
            height + 16.0
        } else {
            0.0
        };

        ui.horizontal_centered(|ui| {
            if ui
                .add(
                    egui::Button::new("➕")
                        .min_size(vec2(32.0, 32.0))
                        .corner_radius(CornerRadius::same(u8::MAX)),
                )
                .on_hover_text_at_pointer("Pick files")
                .clicked()
            {
                action = ChatAction::PickFiles { id: self.id() };
            }
            ui.with_layout(
                Layout::left_to_right(Align::Center).with_main_justify(true),
                |ui| {
                    let Self {
                        chatbox_highlighter: highlighter,
                        ..
                    } = self;
                    let mut layouter = |ui: &egui::Ui, easymark: &str, wrap_width: f32| {
                        let mut layout_job = highlighter.highlight(ui.style(), easymark);
                        layout_job.wrap.max_width = wrap_width;
                        ui.fonts(|f| f.layout_job(layout_job))
                    };

                    self.chatbox_height = egui::TextEdit::multiline(&mut self.chatbox)
                        .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                        .hint_text("Ask me anything…")
                        .layouter(&mut layouter)
                        .show(ui)
                        .response
                        .rect
                        .height()
                        + images_height;
                    if !is_generating
                        && ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.is_none())
                    {
                        self.send_message(settings);
                    }
                },
            );
        });

        if is_max_height {
            ui.add_space(8.0);
        }

        action
    }

    #[inline]
    pub fn flower_active(&self) -> bool {
        self.flower.is_active()
    }

    pub fn poll_flower(&mut self, modal: &mut Modal) {
        let mut last_processed_idx = self.messages.len().saturating_sub(1);

        self.flower
            .extract(|(idx, part)| {
                last_processed_idx = idx;
                let model = self
                    .messages
                    .get(idx - 1)
                    .map_or(GeminiModel::default(), |m| m.model);

                match part {
                    Part::text(data) => {
                        // Safely use unwrap, as we always add
                        // a placeholder message in send_message before running.
                        let current_response_msg = self.messages.last_mut().unwrap();

                        if *data.thought() {
                            // This is a thought
                            if !current_response_msg.is_thought {
                                // If this is the first part of a "thought", turn our
                                // placeholder message into a full "thought" message.
                                current_response_msg.is_thought = true;
                            }
                            // Just append the "thought" text.
                            current_response_msg.content.push_str(data.text());
                        } else {
                            if current_response_msg.is_thought {
                                // "Thoughts" have just ended. Turn off the spinner for them.
                                current_response_msg.is_generating = false;

                                // And create a NEW, separate message for the final answer.
                                // This will keep the thought block on screen.
                                let model = current_response_msg.model;
                                let mut answer_message =
                                    Message::assistant(data.text().into(), model);
                                answer_message.is_generating = true; // It has its own spinner.
                                self.messages.push(answer_message);
                            } else {
                                // Either there were no "thoughts", or this is a continuation of the answer.
                                // Just append the text to the current last message.
                                current_response_msg.content.push_str(data.text());
                            }
                        }
                    }
                    _ => todo!(),
                }
            })
            .finalize(|result| {
                if let Ok((idx, _)) = result {
                } else if let Err(e) = result {
                    let (idx, msg) = match e {
                        Compact::Panicked(e) => {
                            (self.messages.len() - 1, format!("Tokio task panicked: {e}"))
                        }
                        Compact::Suppose((idx, e)) => (idx, e),
                    };

                    let mut clean_msg = msg
                        .strip_prefix("StatusNotOk(\"")
                        .unwrap_or(&msg)
                        .to_string();
                    if clean_msg.ends_with("\")") {
                        clean_msg.pop();
                        clean_msg.pop();
                    }
                    let formatted_msg = clean_msg.replace("\\n", "\n").replace("\\\"", "\"");
                    let final_msg = match serde_json::from_str::<serde_json::Value>(&formatted_msg)
                    {
                        Ok(json_value) => {
                            serde_json::to_string_pretty(&json_value).unwrap_or(formatted_msg)
                        }
                        Err(_) => formatted_msg,
                    };

                    let message = &mut self.messages[idx];
                    message.content = final_msg.clone();
                    message.is_error = true;
                    modal
                        .dialog()
                        .with_body(final_msg)
                        .with_title("Failed to generate completion!")
                        .with_icon(Icon::Error)
                        .open();
                    message.is_generating = false;
                }

                if let Some(last_msg) = self.messages.last_mut() {
                    if last_msg.is_generating {
                        last_msg.is_generating = false;
                    }
                }
            });
    }

    pub fn last_message_contents(&self) -> Option<String> {
        for message in self.messages.iter().rev() {
            if message.content.is_empty() {
                continue;
            }
            return Some(if message.is_user() {
                format!("You: {}", message.content)
            } else {
                message.content.to_string()
            });
        }
        None
    }

    fn stop_generating_button(&self, ui: &mut egui::Ui, radius: f32, pos: Pos2) {
        let rect = Rect::from_min_max(pos + vec2(-radius, -radius), pos + vec2(radius, radius));
        let (hovered, primary_clicked) = ui.input(|i| {
            (
                i.pointer
                    .interact_pos()
                    .map(|p| rect.contains(p))
                    .unwrap_or(false),
                i.pointer.primary_clicked(),
            )
        });
        if hovered && primary_clicked {
            self.stop_generating.store(true, Ordering::SeqCst);
        } else {
            ui.painter().circle(
                pos,
                radius,
                if hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    if ui.style().visuals.dark_mode {
                        let c = ui.style().visuals.faint_bg_color;
                        Color32::from_rgb(c.r(), c.g(), c.b())
                    } else {
                        Color32::WHITE
                    }
                } else {
                    ui.style().visuals.window_fill
                },
                Stroke::new(2.0, ui.style().visuals.window_stroke.color),
            );
            ui.painter().rect_stroke(
                rect.shrink(radius / 2.0 + 1.2),
                2.0,
                Stroke::new(2.0, Color32::DARK_GRAY),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn show_chat_scrollarea(
        &mut self,
        ui: &mut egui::Ui,
        settings: &Settings,
        commonmark_cache: &mut CommonMarkCache,
        #[cfg(feature = "tts")] tts: SharedTts,
    ) -> Option<usize> {
        let mut new_speaker: Option<usize> = None;
        let mut any_prepending = false;
        let mut regenerate_response_idx = None;
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add_space(16.0);
                self.virtual_list
                    .ui_custom_layout(ui, self.messages.len(), |ui, index| {
                        let Some(message) = self.messages.get_mut(index) else {
                            return 0;
                        };
                        let prev_speaking = message.is_speaking;
                        if any_prepending && message.is_prepending {
                            message.is_prepending = false;
                        }
                        let action = message.show(
                            ui,
                            commonmark_cache,
                            #[cfg(feature = "tts")]
                            tts.clone(),
                            index,
                            &mut self.prepend_buf,
                        );
                        match action {
                            MessageAction::None => (),
                            MessageAction::Retry(idx) => {
                                self.retry_message_idx = Some(idx);
                            }
                            MessageAction::Regenerate(idx) => {
                                regenerate_response_idx = Some(idx);
                            }
                        }
                        any_prepending |= message.is_prepending;
                        if !prev_speaking && message.is_speaking {
                            new_speaker = Some(index);
                        }
                        1 // 1 rendered item per row
                    });
            });
        if let Some(regenerate_idx) = regenerate_response_idx {
            self.regenerate_response(settings, regenerate_idx);
        }
        new_speaker
    }

    fn send_text(&mut self, settings: &Settings, text: &str) {
        self.chatbox = text.to_owned();
        self.send_message(settings);
    }

    fn show_suggestions(&mut self, ui: &mut egui::Ui, settings: &Settings) {
        // todo broken weird shit :p
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            widgets::centerer(ui, |ui| {
                let avail_width = ui.available_rect_before_wrap().width() - 24.0;
                ui.horizontal(|ui| {
                    ui.heading(format!(
                        "{}",
                        self.model_picker.selected.to_string().replace("-", " ")
                    )); // todo improve it
                });
                egui::Grid::new("suggestions_grid")
                    .num_columns(3)
                    .max_col_width((avail_width / 2.0).min(200.0))
                    .spacing(vec2(6.0, 6.0))
                    .show(ui, |ui| {
                        // TODO change it
                        if widgets::suggestion(ui, "Tell me a fun fact", "about the Roman empire")
                            .clicked()
                        {
                            self.send_text(settings, "Tell me a fun fact about the Roman empire");
                        }
                        if widgets::suggestion(
                            ui,
                            "Show me a code snippet",
                            "of a web server in Rust",
                        )
                        .clicked()
                        {
                            self.send_text(
                                settings,
                                "Show me a code snippet of a web server in Rust",
                            );
                        }
                        widgets::dummy(ui);
                        ui.end_row();

                        if widgets::suggestion(ui, "Tell me a joke", "about crabs").clicked() {
                            self.send_text(settings, "Tell me a joke about crabs");
                        }
                        if widgets::suggestion(ui, "Give me ideas", "for a birthday present")
                            .clicked()
                        {
                            self.send_text(settings, "Give me ideas for a birthday present");
                        }
                        widgets::dummy(ui);
                        ui.end_row();
                    });
            });
        });
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        settings: &Settings,
        #[cfg(feature = "tts")] tts: SharedTts,
        #[cfg(feature = "tts")] stopped_speaking: bool,
        commonmark_cache: &mut CommonMarkCache,
    ) -> ChatAction {
        let avail = ctx.available_rect();
        let max_height = avail.height() * 0.4 + 24.0;
        let chatbox_panel_height = self.chatbox_height + 24.0;
        let actual_chatbox_panel_height = chatbox_panel_height.min(max_height);
        let is_generating = self.flower_active();
        let mut action = ChatAction::None;

        egui::TopBottomPanel::bottom("chatbox_panel")
            .exact_height(actual_chatbox_panel_height)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    action = self.show_chatbox(
                        ui,
                        chatbox_panel_height >= max_height,
                        is_generating,
                        settings,
                    );
                });
            });

        #[cfg(feature = "tts")]
        let mut new_speaker: Option<usize> = None;

        egui::CentralPanel::default()
            .frame(Frame::central_panel(&ctx.style()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 3,
            }))
            .show(ctx, |ui| {
                // ui.ctx().set_debug_on_hover(true); // TODO DEBUG
                if self.messages.is_empty() {
                    self.show_suggestions(ui, settings);
                } else {
                    #[allow(unused_variables)]
                    if let Some(new) = self.show_chat_scrollarea(
                        ui,
                        settings,
                        commonmark_cache,
                        #[cfg(feature = "tts")]
                        tts,
                    ) {
                        #[cfg(feature = "tts")]
                        {
                            new_speaker = Some(new);
                        }
                    }

                    // stop generating button
                    if is_generating {
                        self.stop_generating_button(
                            ui,
                            16.0,
                            pos2(
                                ui.cursor().max.x - 32.0,
                                avail.height() - 32.0 - actual_chatbox_panel_height,
                            ),
                        );
                    }
                }
            });

        #[cfg(feature = "tts")]
        {
            if let Some(new_idx) = new_speaker {
                log::debug!("new speaker {new_idx} appeared, updating message icons");
                for (i, msg) in self.messages.iter_mut().enumerate() {
                    if i == new_idx {
                        continue;
                    }
                    msg.is_speaking = false;
                }
            }
            if stopped_speaking {
                log::debug!("TTS stopped speaking, updating message icons");
                for msg in self.messages.iter_mut() {
                    msg.is_speaking = false;
                }
            }
        }

        action
    }
}
