//! Audio and subtitle tracks, and their names for the menu.

use gst::prelude::*;

pub struct Track {
    pub id: String,
    pub label: String,
}

#[derive(Default)]
pub struct Tracks {
    pub video: Vec<String>,
    pub audio: Vec<Track>,
    pub text: Vec<Track>,
}

impl Tracks {
    pub fn from_collection(collection: &gst::StreamCollection) -> Self {
        let mut tracks = Self::default();
        for stream in collection.iter() {
            let Some(id) = stream.stream_id().map(String::from) else {
                continue;
            };
            let kind = stream.stream_type();
            if kind.contains(gst::StreamType::VIDEO) {
                tracks.video.push(id);
            } else if kind.contains(gst::StreamType::AUDIO) {
                tracks.audio.push(Track { id, label: describe(&stream) });
            } else if kind.contains(gst::StreamType::TEXT) {
                tracks.text.push(Track { id, label: describe(&stream) });
            }
        }
        number_duplicates(&mut tracks.audio, "Track");
        number_duplicates(&mut tracks.text, "Subtitles");
        tracks
    }

    /// Worth showing a menu only when there is something to choose.
    pub fn has_choices(&self) -> bool {
        self.audio.len() > 1 || !self.text.is_empty()
    }
}

/// "Hindi", "English", or the track's own title if it has no language.
fn describe(stream: &gst::Stream) -> String {
    let tags = stream.tags();
    let tag = |name: &str| {
        tags.as_ref()
            .and_then(|tags| tags.generic(name))
            .and_then(|value| value.get::<String>().ok())
            .filter(|value| !value.is_empty())
    };
    let language = tag(gst::tags::LanguageCode::TAG_NAME).map(|code| {
        gst_tag::language_codes::language_name(&code)
            .map(|name| name.to_string())
            .unwrap_or_else(|| code.to_uppercase())
    });
    language.or_else(|| tag(gst::tags::Title::TAG_NAME)).unwrap_or_default()
}

/// Unnamed tracks become "Track 1", "Track 2"; repeated names get a number.
fn number_duplicates(tracks: &mut [Track], fallback: &str) {
    let labels: Vec<String> = tracks.iter().map(|t| t.label.clone()).collect();
    for (index, track) in tracks.iter_mut().enumerate() {
        if track.label.is_empty() {
            track.label = format!("{fallback} {}", index + 1);
        } else if labels.iter().filter(|l| **l == track.label).count() > 1 {
            let nth = labels[..=index].iter().filter(|l| **l == track.label).count();
            track.label = format!("{} {nth}", track.label);
        }
    }
}
