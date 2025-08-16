use anyhow::Result;
use base64::Engine;
use eframe::egui::{self, vec2, Color32, Rect, RichText, Stroke};
use gemini_client_api::gemini::types::request::{InlineData, Part};
use image::ImageFormat;
use std::{
    fs::File,
    io::{BufReader, Cursor},
    path::{Path, PathBuf},
};

pub async fn convert_image_to_part(path: &Path) -> Result<Part> {
    // Read the entire file into a byte vector asynchronously.
    let image_bytes = tokio::fs::read(path).await?;

    // Guess the image format from the byte slice.
    let format = image::guess_format(&image_bytes)?;
    let mime_type = mime_guess::from_path(path).first_or_octet_stream();

    // The final bytes to be encoded. This might be the original bytes or converted bytes.
    let final_bytes = if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        log::debug!("got {format:?} image, converting to png");
        // image::load needs a reader that implements Read + Seek. A Cursor is perfect for this.
        let reader = Cursor::new(&image_bytes);
        let img = image::load(reader, format)?;

        // Write the converted image (as PNG) into a new buffer.
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
        buf
    } else {
        // No conversion needed, use the original bytes.
        image_bytes
    };

    let base64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);
    log::debug!(
        "converted image to {} bytes of base64 with mime type {}",
        base64.len(),
        mime_type
    );
    Ok(Part::inline_data(InlineData::new(
        mime_type.to_string(),
        base64,
    )))
}

pub fn show_images(ui: &mut egui::Ui, images: &mut Vec<PathBuf>, mutate: bool) {
    const MAX_IMAGE_HEIGHT: f32 = 128.0;
    let pointer_pos = ui.input(|i| i.pointer.interact_pos());
    let mut showing_x = false;

    images.retain_mut(|image_path| {
        let path_string = image_path.display().to_string();
        let resp = ui
            .group(|ui| {
                ui.vertical(|ui| {
                    ui.add(
                        egui::Image::new(format!("file://{path_string}"))
                            .max_height(MAX_IMAGE_HEIGHT)
                            .fit_to_original_size(1.0),
                    )
                    .on_hover_text(path_string);

                    let file_name = image_path.file_name().unwrap_or_default().to_string_lossy();
                    ui.add(egui::Label::new(RichText::new(file_name).small()).truncate());
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
