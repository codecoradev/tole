//! The images queued for the next message (#560): the list the host last
//! reported and the row that shows it.

use crate::{clip, App, ImageItem};

impl App {
    /// The images queued for the next message, as the host last reported.
    pub fn images(&self) -> &[ImageItem] {
        &self.images
    }

    /// Replace the queued-image list with what the host reports.
    pub fn set_images(&mut self, images: Vec<ImageItem>) {
        self.images = images;
    }

    /// The row for queued images (#560), or `None` when nothing is attached.
    pub fn image_row(&self, width: usize) -> Option<String> {
        let row = match self.images.as_slice() {
            [] => return None,
            [one] => format!("image: {} · next message", one.label()),
            many => format!(
                "images ({}): {} · next message",
                many.len(),
                many.iter()
                    .map(ImageItem::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Some(clip(&row, width.saturating_sub(1)))
    }

    /// The host's queue goes with the message that starts a turn.
    pub(crate) fn images_sent(&mut self) {
        self.images_in_flight = !self.images.is_empty();
        self.images.clear();
    }
}
