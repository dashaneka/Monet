//! Remembers where playback stopped in each video.

use std::path::PathBuf;

use gtk::glib;

const GROUP: &str = "positions";
/// Positions earlier than this aren't worth resuming.
const MIN_POSITION: f64 = 10.0;
/// Stopping this close to the end counts as finished.
pub const END_MARGIN: f64 = 15.0;

fn path() -> PathBuf {
    glib::user_data_dir().join("monet").join("positions.ini")
}

/// Hashed so the file doesn't list the names of everything you've watched.
fn key(uri: &str) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, uri)
        .map(|sum| sum.to_string())
        .unwrap_or_else(|| uri.to_owned())
}

fn read() -> glib::KeyFile {
    let file = glib::KeyFile::new();
    let _ = file.load_from_file(path(), glib::KeyFileFlags::NONE);
    file
}

fn write(file: &glib::KeyFile) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = file.save_to_file(&path) {
        eprintln!("monet: could not save playback position: {err}");
    }
}

pub fn load(uri: &str) -> Option<f64> {
    read()
        .double(GROUP, &key(uri))
        .ok()
        .filter(|&position| position >= MIN_POSITION)
}

pub fn save(uri: &str, position: f64, duration: Option<f64>) {
    let finished = duration.is_some_and(|duration| position > duration - END_MARGIN);
    if position < MIN_POSITION || finished {
        forget(uri);
        return;
    }
    let file = read();
    file.set_double(GROUP, &key(uri), position);
    write(&file);
}

pub fn forget(uri: &str) {
    let file = read();
    if file.remove_key(GROUP, &key(uri)).is_ok() {
        write(&file);
    }
}
