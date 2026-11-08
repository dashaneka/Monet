//! Recently played videos. They live in GNOME's own recent files list, so
//! turning off file history in Settings covers Monet too.

use gtk::glib;
use gtk::prelude::*;

/// How many show on the empty screen.
const SHOWN: usize = 5;

pub fn add(uri: &str) {
    gtk::RecentManager::default().add_item(uri);
}

/// Monet's recent videos that still exist, newest first.
pub fn list() -> Vec<gtk::RecentInfo> {
    let app = glib::application_name()
        .or_else(glib::prgname)
        .unwrap_or_else(|| "monet".into());
    let mut items: Vec<gtk::RecentInfo> = gtk::RecentManager::default()
        .items()
        .into_iter()
        .filter(|item| item.has_application(&app) && item.exists())
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.modified().to_unix()));
    items.truncate(SHOWN);
    items
}
