use anyhow::{anyhow, Result};
use base64::Engine;
use eframe::egui::{self, vec2, Color32, Rect, RichText, Stroke};
use gemini_client_api::gemini::types::request::{InlineData, Part};
use image::ImageFormat;
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

const GEMINI_MIME: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "audio/aac",
    "audio/flac",
    "audio/mp3",
    "audio/m4a",
    "audio/mpeg",
    "audio/mpga",
    "audio/opus",
    "audio/pcm",
    "audio/wav",
    "audio/webm",
    "audio/aiff",
    "audio/ogg",
    "video/mp4",
    "application/pdf",
    "text/plain",
];

pub async fn convert_file_to_part(path: &Path) -> Result<Part> {
    // Asynchronously read the file into bytes
    let file_bytes = tokio::fs::read(path).await?;

    // Determine the MIME type of the file
    let mime_type = mime_guess::from_path(path).first_or_octet_stream();
    let mut mime_str = mime_type.to_string();

    if &mime_str == "application/json" {
        mime_str = "text/plain".to_string();
    }

    log::info!(
        "Processing file: {}, MIME type: {}",
        path.display(),
        mime_str
    );

    // For images that are not PNG/JPEG, convert them to PNG for better compatibility.
    // For video and text files, we simply send them "as is".
    let final_bytes = if mime_type.type_() == "image" {
        match image::guess_format(&file_bytes) {
            Ok(format) if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) => {
                log::debug!("Got {format:?} image, converting to png");
                mime_str = "image/png".to_string();
                let reader = Cursor::new(&file_bytes);
                let img = image::load(reader, format)?;
                let mut buf = Vec::new();
                img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
                buf
            }
            _ => {
                // It's already PNG/JPEG or an unknown image format, send as is
                file_bytes
            }
        }
    } else {
        // For video, text, and other file types, use the original bytes
        file_bytes
    };

    if !GEMINI_MIME.contains(&mime_str.as_str()) {
        return Err(anyhow!(
            "Unsupported MIME type: {}. Supported types are: {:?}",
            mime_str,
            GEMINI_MIME
        ));
    }

    // Encode the final bytes in Base64
    let base64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);
    log::debug!(
        "Converted file to {} bytes of base64 with mime type {}",
        base64.len(),
        mime_str
    );

    // Create a Part for the API
    Ok(Part::inline_data(InlineData::new(mime_str, base64)))
}

pub fn show_files(ui: &mut egui::Ui, files: &mut Vec<PathBuf>, mutate: bool) {
    const MAX_PREVIEW_HEIGHT: f32 = 128.0;
    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
    let mut showing_x = false;

    files.retain_mut(|file_path| {
        let path_string = file_path.display().to_string();
        let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream();

        let is_exist = file_path.exists();
        let frame_color = if is_exist { egui::Color32::GRAY } else { egui::Color32::ORANGE };
        let custom_frame = egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.0, frame_color));

        let resp = custom_frame
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    // Display preview or icon depending on the file type
                    match mime_type.type_().as_str() {
                        "image" if is_exist => {
                            ui.add(
                                egui::Image::new(format!("file://{path_string}"))
                                    .max_height(MAX_PREVIEW_HEIGHT)
                                    .fit_to_original_size(1.0),
                            );
                        }
                        _ => {
                            // Create a container-frame with a fixed height
                            egui::Frame::NONE.show(ui, |ui| {
                                // Force the height to be equal to the maximum preview image height
                                ui.set_height(MAX_PREVIEW_HEIGHT);
                                // Set the width so that the widget is not too narrow
                                ui.set_width(MAX_PREVIEW_HEIGHT * 1.2);

                                // Center the icon inside this frame
                                ui.centered_and_justified(|ui| {
                                    let icon = if !is_exist {
                                        "⚠"
                                    } else {
                                        match mime_type.type_().as_str() {
                                            "video" => "🎬",
                                            "audio" => "🎶",
                                            // "text" => "",
                                            _ => "📎",
                                        }
                                    };

                                    ui.label(RichText::new(icon).size(40.0));
                                });
                            });
                        }
                    }

                    let mut text = file_path.file_name().unwrap_or_default().to_string_lossy();
                    if !is_exist {
                        text.to_mut().push_str(" (FILE NOT FOUND)");
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(text).small(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(path_string);
                });
            })
            .response;

        if !mutate || showing_x {
            return true;
        }

        if let Some(pos) = pointer_pos {
            if resp.rect.expand(8.0).contains(pos) {
                showing_x = true;

                // render an ❌ in a red circle
                let top = resp.rect.right_top();
                let x_rect = Rect::from_center_size(top, vec2(16.0, 16.0));
                let contains_pointer = x_rect.contains(pos);

                ui.painter()
                    .circle_filled(top, 10.0, ui.visuals().window_fill);
                ui.painter().circle_filled(
                    top,
                    8.0,
                    if contains_pointer {
                        ui.visuals().gray_out(ui.visuals().error_fg_color)
                    } else {
                        ui.visuals().error_fg_color
                    },
                );
                ui.painter().line_segment(
                    [top - vec2(3.0, 3.0), top + vec2(3.0, 3.0)],
                    Stroke::new(2.0, Color32::WHITE),
                );
                ui.painter().line_segment(
                    [top - vec2(3.0, -3.0), top + vec2(3.0, -3.0)],
                    Stroke::new(2.0, Color32::WHITE),
                );

                if contains_pointer && ui.input(|i| i.pointer.primary_clicked()) {
                    return false;
                }
            }
        }

        true
    });
}
