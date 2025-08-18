use crate::{
    chat::{Chat, ChatAction, ChatExportFormat},
    widgets::{ModelPicker, RequestInfoType, Settings},
};
use eframe::egui::{self, vec2, Color32, CornerRadius, Frame, Layout, Stroke};
use egui_commonmark::CommonMarkCache;
use egui_modal::{Icon, Modal};
use egui_notify::{Toast, Toasts};
use egui_twemoji::EmojiLabel;
use egui_virtual_list::VirtualList;
use flowync::{CompactFlower, CompactHandle};
use gemini_client_api::gemini::ask::Gemini;
#[cfg(feature = "tts")]
use parking_lot::RwLock;
#[cfg(feature = "tts")]
use std::sync::Arc;
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, time::Instant};
#[cfg(feature = "tts")]
use tts::Tts;

#[derive(Default, PartialEq, serde::Serialize, serde::Deserialize)]
enum SessionTab {
    #[default]
    Chats,
}

#[cfg(feature = "tts")]
pub type SharedTts = Option<Arc<RwLock<Tts>>>;
enum BackendResponse {
    Ignore,
    Toast(Toast),
    Files { id: usize, files: Vec<PathBuf> },
    Settings(Box<Settings>),
}

// <progress, response, error>
type BackendFlower = CompactFlower<(), BackendResponse, String>;
type BackendFlowerHandle = CompactHandle<(), BackendResponse, String>;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Sessions {
    tab: SessionTab,
    chats: Vec<Chat>,
    selected_chat: usize,
    #[serde(skip)]
    chat_marked_for_deletion: usize,
    #[cfg(feature = "tts")]
    #[serde(skip)]
    is_speaking: bool,
    #[cfg(feature = "tts")]
    #[serde(skip)]
    tts: SharedTts,
    #[serde(skip)]
    commonmark_cache: CommonMarkCache,
    #[serde(skip)]
    flower: BackendFlower,
    #[serde(skip)]
    last_request_time: Instant,
    #[serde(skip)]
    virtual_list: Rc<RefCell<VirtualList>>,
    edited_chat: Option<usize>,
    chat_export_format: ChatExportFormat,
    #[serde(skip)]
    toasts: Toasts,
    settings_open: bool,
    pub settings: Settings,
}

impl Default for Sessions {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            tab: SessionTab::Chats,
            chats: vec![Chat::default()],
            selected_chat: 0,
            chat_marked_for_deletion: 0,
            #[cfg(feature = "tts")]
            is_speaking: false,
            #[cfg(feature = "tts")]
            tts: Tts::default()
                .map_err(|e| log::error!("failed to initialize TTS: {e}"))
                .map(|tts| Arc::new(RwLock::new(tts)))
                .ok(),
            commonmark_cache: CommonMarkCache::default(),
            flower: BackendFlower::new(1),
            last_request_time: now,
            virtual_list: Rc::new(RefCell::new({
                let mut list = VirtualList::new();
                list.check_for_resize(false);
                list
            })),
            edited_chat: None,
            chat_export_format: ChatExportFormat::default(),
            toasts: Toasts::default(),
            settings_open: false,
            settings: Settings::default(),
        }
    }
}

async fn pick_files(id: usize, handle: &BackendFlowerHandle) {
    let Some(files) = rfd::AsyncFileDialog::new()
        .add_filter(
            "Media & Text",
            &[
                crate::IMAGE_FORMATS,
                crate::VIDEO_FORMATS,
                crate::MUSIC_FORMATS,
                crate::TEXT_FORMATS,
            ]
            .concat(),
        )
        .add_filter("Image", crate::IMAGE_FORMATS)
        .add_filter("Video", crate::VIDEO_FORMATS)
        .add_filter("Text", crate::TEXT_FORMATS)
        .add_filter("Music", crate::MUSIC_FORMATS)
        .pick_files()
        .await
    else {
        handle.success(BackendResponse::Ignore);
        return;
    };

    log::info!("selected {} file(s)", files.len());

    handle.success(BackendResponse::Files {
        id,
        files: files.iter().map(|f| f.path().to_path_buf()).collect(),
    });
}

async fn load_settings(handle: &BackendFlowerHandle) {
    let Some(file) = rfd::AsyncFileDialog::new()
        .add_filter("JSON file", &["json"])
        .pick_file()
        .await
    else {
        handle.success(BackendResponse::Toast(Toast::info("No file selected")));
        return;
    };

    log::info!("reading settings from `{}`", file.path().display());
    let Ok(f) = std::fs::File::open(file.path()).map_err(|e| {
        log::error!("failed to open file `{}`: {e}", file.path().display());
        handle.success(BackendResponse::Toast(Toast::error(e.to_string())));
    }) else {
        return;
    };

    let settings = serde_json::from_reader(std::io::BufReader::new(f));
    if let Ok(settings) = settings {
        handle.success(BackendResponse::Settings(settings));
    } else if let Err(e) = settings {
        log::error!("failed to load settings: {e}");
        handle.success(BackendResponse::Toast(Toast::error(e.to_string())));
    }
}

fn preview_files_being_dropped(ctx: &egui::Context) {
    use egui::*;
    use std::fmt::Write as _;

    if !ctx.input(|i| i.raw.hovered_files.is_empty()) {
        let text = ctx.input(|i| {
            let mut text = "Dropping files:".to_owned();
            for file in &i.raw.hovered_files {
                if let Some(path) = &file.path {
                    write!(text, "\n{}", path.display()).ok();
                } else if !file.mime.is_empty() {
                    write!(text, "\n{}", file.mime).ok();
                } else {
                    text += "\n???";
                }
            }
            text
        });

        let painter =
            ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("file_drop_target")));

        let screen_rect = ctx.screen_rect();
        painter.rect_filled(screen_rect, 0.0, Color32::from_black_alpha(192));
        painter.text(
            screen_rect.center(),
            Align2::CENTER_CENTER,
            text,
            TextStyle::Heading.resolve(&ctx.style()),
            Color32::WHITE,
        );
    }
}

impl Sessions {
    pub fn new() -> Self {
        // todo
        Self::default()
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        // check if tts stopped speaking
        #[cfg(feature = "tts")]
        let prev_is_speaking = self.is_speaking;
        #[cfg(feature = "tts")]
        {
            self.is_speaking = if let Some(tts) = &self.tts {
                tts.read().is_speaking().unwrap_or(false)
            } else {
                false
            };
        }

        // if speaking, continuously check if stopped
        #[cfg(feature = "tts")]
        let mut request_repaint = self.is_speaking;

        #[cfg(not(feature = "tts"))]
        let mut request_repaint = false;

        let mut modal = Modal::new(ctx, "sessions_main_modal");
        let mut chat_modal = Modal::new(ctx, "chat_main_modal").with_close_on_outside_click(true);
        let settings_modal =
            Modal::new(ctx, "global_settings_modal").with_close_on_outside_click(true);

        chat_modal.show_dialog();
        modal.show_dialog();
        self.settings.show_modal(&settings_modal);

        let avail_width = ctx.available_rect().width();
        egui::SidePanel::left("sessions_panel")
            .resizable(true)
            .max_width(avail_width * 0.5)
            .show(ctx, |ui| {
                self.show_left_panel(ui);
                ui.allocate_space(ui.available_size());
            });

        // poll all flowers
        for chat in self.chats.iter_mut() {
            if chat.flower_active() {
                request_repaint = true;
                chat.poll_flower(&mut chat_modal);
            }
        }
        if self.flower.is_active() {
            request_repaint = true;
            self.poll_backend_flower(&modal);
        }

        if request_repaint {
            ctx.request_repaint();
        }

        if self.settings_open {
            self.edited_chat = None;
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    self.settings.show(
                        ui,
                        &mut |typ| match typ {
                            RequestInfoType::LoadSettings => {
                                let handle = self.flower.handle();
                                tokio::spawn(async move {
                                    handle.activate();
                                    load_settings(&handle).await;
                                });
                            }
                        },
                        &settings_modal,
                    );
                });
            });
        } else if let Some(edited_chat) = self.edited_chat {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    self.show_chat_edit_panel(ui, edited_chat);
                })
            });
        } else {
            self.show_selected_chat(
                ctx,
                #[cfg(feature = "tts")]
                (prev_is_speaking && !self.is_speaking),
            );
            preview_files_being_dropped(ctx);
        }

        // display toast queue
        self.toasts.show(ctx);
    }

    fn show_selected_chat(
        // here: main chat
        &mut self,
        ctx: &egui::Context,
        #[cfg(feature = "tts")] stopped_talking: bool,
    ) {
        let Some(chat) = self.chats.get_mut(self.selected_chat) else {
            self.selected_chat = 0;
            return;
        };

        ctx.input(|i| {
            for file in &i.raw.dropped_files {
                if let Some(path) = &file.path {
                    let filename = path.file_name().unwrap_or_default().to_string_lossy();
                    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                        log::warn!("dropped file `{}` has no extension", path.display());
                        self.toasts.add(Toast::info(format!(
                            "Skipping file with no extension: `{filename}`"
                        )));
                        continue;
                    };

                    let all_formats: Vec<_> = [
                        crate::IMAGE_FORMATS,
                        crate::VIDEO_FORMATS,
                        crate::TEXT_FORMATS,
                        crate::MUSIC_FORMATS,
                    ].concat();
                    
                    if !all_formats.contains(&ext.to_lowercase().as_str()) {
                        log::warn!(
                            "dropped file `{}` has unsupported extension `{ext}`",
                            path.display()
                        );
                        self.toasts.add(Toast::info(format!(
                            "Skipping unsupported file type: `{filename}`"
                        )));
                        continue;
                    }
                    chat.files.push(path.clone());
                }
            }
        });

        let action = chat.show(
            ctx,
            &self.settings,
            #[cfg(feature = "tts")]
            self.tts.clone(),
            #[cfg(feature = "tts")]
            stopped_talking,
            &mut self.commonmark_cache,
        );

        match action {
            ChatAction::None => (),
            ChatAction::PickFiles { id } => {
                let handle = self.flower.handle();
                tokio::spawn(async move {
                    handle.activate();
                    pick_files(id, &handle).await;
                });
            }
        }
    }

    fn show_remove_chat_modal_inner(&mut self, ui: &mut egui::Ui, modal: &Modal) {
        modal.title(ui, "Remove Chat");
        modal.frame(ui, |ui| {
            modal.body_and_icon(
                ui,
                "Do you really want to remove this chat? \
                You cannot undo this action later.\n\
                Hold Shift to surpass this warning.",
                Icon::Warning,
            );
            modal.buttons(ui, |ui| {
                if modal.button(ui, "No").clicked() {
                    modal.close();
                }
                let summary = self
                    .chats
                    .get(self.chat_marked_for_deletion)
                    .map(|c| {
                        if c.summary.is_empty() {
                            "New Chat"
                        } else {
                            c.summary.as_str()
                        }
                    })
                    .unwrap_or("New Chat");
                if modal
                    .caution_button(ui, "Yes")
                    .on_hover_text(format!("Remove chat \"{summary}\"",))
                    .clicked()
                {
                    modal.close();
                    self.remove_chat(self.chat_marked_for_deletion);
                }
            });
        });
    }

    fn show_chat_edit_panel(&mut self, ui: &mut egui::Ui, chat_idx: usize) {
        ui.horizontal(|ui| {
            let Some(chat) = self.chats.get(chat_idx) else {
                return;
            };
            if chat.summary.is_empty() {
                ui.heading("Editing Chat \"New Chat\"");
            } else {
                ui.heading(format!("Editing Chat \"{}\"", chat.summary));
            }

            ui.with_layout(Layout::right_to_left(egui::Align::Min), |ui| {
                if ui
                    .add(
                        egui::Button::new("❌")
                            .fill(Color32::TRANSPARENT)
                            .frame(false),
                    )
                    .on_hover_text("Close")
                    .clicked()
                {
                    self.edited_chat = None;
                }
            });
        });

        egui::CollapsingHeader::new("Model")
            .default_open(true)
            .show(ui, |ui| {
                let Some(chat) = self.chats.get_mut(chat_idx) else {
                    return;
                };

                chat.model_picker.show(ui, &mut |_| {});

                if self.settings.inherit_chat_picker {
                    self.settings.model_picker.selected = chat.model_picker.selected.clone();
                }
            });
        ui.collapsing("Export", |ui| {
            ui.label("Export chat history to a file");
            let format = self.chat_export_format;
            egui::ComboBox::from_label("Export Format")
                .selected_text(format.to_string())
                .show_ui(ui, |ui| {
                    for format in ChatExportFormat::ALL {
                        ui.selectable_value(
                            &mut self.chat_export_format,
                            format,
                            format.to_string(),
                        );
                    }
                });
            if ui.button("Save As…").clicked() {
                let task = rfd::AsyncFileDialog::new()
                    .add_filter(format!("{format:?} file"), format.extensions())
                    .save_file();
                let Some(chat) = self.chats.get_mut(chat_idx) else {
                    return;
                };
                let messages = chat.messages.clone();
                let handle = self.flower.handle();
                tokio::spawn(async move {
                    let toast = crate::chat::export_messages(messages, format, task)
                        .await
                        .map_err(|e| {
                            log::error!("failed to export messages: {e}");
                            e
                        });

                    handle.activate();
                    if let Ok(toast) = toast {
                        handle.success(BackendResponse::Toast(toast))
                    } else if let Err(e) = toast {
                        handle.success(BackendResponse::Toast(Toast::error(e.to_string())))
                    };
                });
            }
        });
    }

    fn show_left_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(ui.style().spacing.window_margin.top as _);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, SessionTab::Chats, "Chats");
            ui.with_layout(Layout::right_to_left(egui::Align::Max), |ui| {
                ui.toggle_value(&mut self.settings_open, "⚙")
                    .on_hover_text("Settings");
            });
        });

        ui.add_space(8.0);

        match self.tab {
            SessionTab::Chats => {
                let modal = Modal::new(ui.ctx(), "remove_chat_modal");
                self.show_chats(ui, &modal);
                modal.show(|ui| {
                    self.show_remove_chat_modal_inner(ui, &modal);
                });
            }
        }
    }

    #[inline]
    pub fn model_picker(&self) -> &ModelPicker {
        &self.settings.model_picker
    }

    fn poll_backend_flower(&mut self, modal: &Modal) {
        self.flower.extract(|()| ()).finalize(|resp| {
            match resp {
                Ok(BackendResponse::Ignore) => (),
                Ok(BackendResponse::Toast(toast)) => {
                    self.toasts.add(toast);
                }
                Ok(BackendResponse::Files { id, files }) => {
                    if let Some(chat) = self.chats.iter_mut().find(|c| c.id() == id) {
                        log::debug!("adding {} file(s) to chat {}", files.len(), id);
                        chat.files.extend(files);
                    }
                }
                Ok(BackendResponse::Settings(settings)) => {
                    self.settings = *settings;
                }
                Err(flowync::error::Compact::Suppose(e)) => {
                    modal
                        .dialog()
                        .with_icon(Icon::Error)
                        .with_title("Request failed")
                        .with_body(e)
                        .open();
                }
                Err(flowync::error::Compact::Panicked(e)) => {
                    log::error!("task panicked: {e}");
                    modal
                        .dialog()
                        .with_icon(Icon::Error)
                        .with_title("Task panicked")
                        .with_body(format!("Task panicked: {e}"))
                        .open();
                }
            };
        });
    }

    #[inline]
    fn add_default_chat(&mut self) {
        // id 1 is already used, and we (probably) don't want to reuse ids for flowers
        self.chats
            .push(Chat::new(self.chats.len() + 2, self.model_picker().clone()));
    }

    fn remove_chat(&mut self, idx: usize) {
        self.chats.remove(idx);
        if self.chats.is_empty() {
            self.add_default_chat();
            self.selected_chat = 0;
        } else if self.selected_chat >= self.chats.len() {
            self.selected_chat = self.chats.len() - 1;
        }
    }

    /// Returns whether any chat was removed
    fn show_chat_frame(&mut self, ui: &mut egui::Ui, idx: usize, modal: &Modal) -> bool {
        let Some(chat) = &self.chats.get(idx) else {
            return false;
        };
        let mut ignore_click = false;

        let last_message = chat
            .last_message_contents()
            .unwrap_or_else(|| "No recent messages".to_string());

        let summary = chat.summary.clone();

        ui.horizontal(|ui| {
            if summary.is_empty() {
                ui.add(egui::Label::new("New Chat").selectable(false).truncate());
            } else {
                EmojiLabel::new(summary)
                    .selectable(false)
                    .truncate()
                    .show(ui);
            }

            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if ui
                    .add(
                        egui::Button::new("❌")
                            .small()
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                    )
                    .on_hover_text("Remove chat")
                    .clicked()
                {
                    if self.chats[idx].messages.is_empty() || ui.input(|i| i.modifiers.shift) {
                        self.remove_chat(idx);
                    } else {
                        self.chat_marked_for_deletion = idx;
                        self.edited_chat = None;
                        modal.open();
                    }
                    ignore_click = true;
                }
                if ui
                    .add(
                        egui::Button::new("\u{270f}")
                            .small()
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                    )
                    .on_hover_text("Edit")
                    .clicked()
                {
                    ignore_click = true;

                    // toggle editing
                    self.edited_chat = if self.edited_chat == Some(idx) {
                        None
                    } else {
                        Some(idx)
                    };
                }
            });
        });

        ui.add_enabled(
            false,
            egui::Label::new(last_message).selectable(false).truncate(),
        );
        ignore_click
    }

    /// Returns whether the chat should be selected as the current one
    fn show_chat_in_sidepanel(&mut self, ui: &mut egui::Ui, idx: usize, modal: &Modal) -> bool {
        let mut ignore_click = false;
        let resp = Frame::group(ui.style())
            .corner_radius(CornerRadius::same(6))
            .stroke(Stroke::new(2.0, ui.style().visuals.window_stroke.color))
            .fill(if self.selected_chat == idx {
                ui.style().visuals.faint_bg_color
            } else {
                ui.style().visuals.window_fill
            })
            .show(ui, |ui| {
                ignore_click = self.show_chat_frame(ui, idx, modal);
            })
            .response;

        // very hacky way to determine if the group has been clicked, for some reason
        // egui doens't register clicked() events on it
        let (primary_clicked, hovered) = if modal.is_open() {
            (false, false)
        } else {
            ui.input(|i| {
                (
                    i.pointer.primary_clicked(),
                    i.pointer
                        .interact_pos()
                        .map(|p| resp.rect.contains(p))
                        .unwrap_or(false),
                )
            })
        };

        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        !ignore_click && primary_clicked && hovered
    }

    fn show_chats(&mut self, ui: &mut egui::Ui, modal: &Modal) {
        ui.vertical_centered_justified(|ui| {
            if ui
                .add(egui::Button::new("➕ New Chat").min_size(vec2(0.0, 24.0)))
                .on_hover_text("Create a new chat")
                .clicked()
            {
                self.add_default_chat();
                self.selected_chat = self.chats.len() - 1;
                self.edited_chat = None;
                self.settings_open = false;
            }
        });

        ui.add_space(2.0);

        let vlist = self.virtual_list.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            vlist
                .borrow_mut()
                .ui_custom_layout(ui, self.chats.len(), |ui, i| {
                    if self.show_chat_in_sidepanel(ui, i, modal) {
                        self.selected_chat = i;
                        self.settings_open = false;
                        self.edited_chat = None;
                    }
                    ui.add_space(2.0);
                    1
                });
        });
    }
}
