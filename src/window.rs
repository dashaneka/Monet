use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::player::{self, Player};

type Slot = Rc<RefCell<Option<Rc<Player>>>>;

pub const MIN_SIZE: (i32, i32) = (360, 220);
/// How long a window opened with a file waits for the video's size before
/// showing anyway, so a slow file or one without video never leaves it hidden.
const SHOW_TIMEOUT: Duration = Duration::from_millis(1500);

pub fn new_window(app: &adw::Application, file: Option<&gio::File>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Monet")
        .default_width(960)
        .default_height(540)
        .width_request(MIN_SIZE.0)
        .height_request(MIN_SIZE.1)
        .build();
    // Narrow windows (portrait videos) drop the volume slider so the seek bar
    // keeps room; the player watches for this breakpoint.
    window.add_breakpoint(adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        540.0,
        adw::LengthUnit::Sp,
    )));

    let slot = Slot::default();

    let open_action = gio::SimpleAction::new("open", None);
    open_action.connect_activate(glib::clone!(
        #[weak]
        window,
        #[strong]
        slot,
        move |_, _| choose_file(&window, &slot)
    ));
    window.add_action(&open_action);

    let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop_target.connect_drop(glib::clone!(
        #[weak]
        window,
        #[strong]
        slot,
        #[upgrade_or]
        false,
        move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let Some(file) = list.files().into_iter().next() else {
                return false;
            };
            load(&window, &slot, &file);
            true
        }
    ));
    window.add_controller(drop_target);

    window.connect_close_request(glib::clone!(
        #[strong]
        slot,
        move |_| {
            let player = slot.borrow_mut().take();
            if let Some(player) = player {
                player.shutdown();
            }
            glib::Propagation::Proceed
        }
    ));

    match file {
        // The player shows the window once it knows the video's shape.
        Some(file) => {
            load(&window, &slot, file);            glib::timeout_add_local_once(
                SHOW_TIMEOUT,
                glib::clone!(
                    #[weak]
                    window,
                    move || show(&window)
                ),
            );
        }
        None => {
            show_empty(&window);
            window.present();
        }
    }
}

/// Presents a window still waiting to be shown; a no-op once visible, so it
/// never steals focus later.
pub fn show(window: &adw::ApplicationWindow) {
    if !window.is_visible() {
        window.present();
    }
}

fn load(window: &adw::ApplicationWindow, slot: &Slot, file: &gio::File) {
    load_at(window, slot, file, None);
}

fn load_at(window: &adw::ApplicationWindow, slot: &Slot, file: &gio::File, start_at: Option<f64>) {
    let previous = slot.borrow_mut().take();
    if let Some(previous) = previous {
        previous.shutdown();
    }

    let title = file
        .basename()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Monet".into());
    window.set_title(Some(&title));

    let uri = file.uri();
    match player::build_pipeline(&uri) {
        Ok((playbin, paintable)) => {
            let reload = {
                let window = window.downgrade();
                let slot = Rc::downgrade(slot);
                let file = file.clone();
                move |position| {
                    if let (Some(window), Some(slot)) = (window.upgrade(), slot.upgrade()) {
                        load_at(&window, &slot, &file, Some(position));
                    }
                }
            };
            let player = Player::new(window, &title, &uri, playbin, &paintable, reload);
            player.start(start_at);
            *slot.borrow_mut() = Some(player);
        }
        Err(err) => show_error(window, &title, &err.to_string()),
    }
}

fn choose_file(window: &adw::ApplicationWindow, slot: &Slot) {
    let videos = gtk::FileFilter::new();
    videos.set_name(Some("Videos"));
    videos.add_mime_type("video/*");
    let all = gtk::FileFilter::new();
    all.set_name(Some("All Files"));
    all.add_pattern("*");

    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&videos);
    filters.append(&all);

    let dialog = gtk::FileDialog::builder()
        .title("Open Video")
        .modal(true)
        .filters(&filters)
        .default_filter(&videos)
        .build();
    if let Some(dir) = glib::user_special_dir(glib::UserDirectory::Videos) {
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
    }

    dialog.open(
        Some(window),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            window,
            #[strong]
            slot,
            move |result| {
                if let Ok(file) = result {
                    load(&window, &slot, &file);
                }
            }
        ),
    );
}

pub fn header_bar(title: &str) -> adw::HeaderBar {
    let open_button = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Open Video (Ctrl+O)")
        .action_name("win.open")
        .build();
    let header = adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new(title, ""))
        .build();
    header.pack_start(&open_button);
    header
}

fn show_page(window: &adw::ApplicationWindow, title: &str, page: &adw::StatusPage) {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar(title));
    toolbar.set_content(Some(page));
    window.set_content(Some(&toolbar));
}

fn show_empty(window: &adw::ApplicationWindow) {
    let open_button = gtk::Button::builder()
        .label("Open Video…")
        .action_name("win.open")
        .halign(gtk::Align::Center)
        .build();
    open_button.add_css_class("pill");
    open_button.add_css_class("suggested-action");

    let page = adw::StatusPage::builder()
        .icon_name("video-x-generic-symbolic")
        .title("Drop a Video Here")
        .description("Or open one from your files")
        .child(&open_button)
        .build();
    show_page(window, "Monet", &page);
}

pub fn show_error(window: &adw::ApplicationWindow, title: &str, message: &str) {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Can't Play This Video")
        .description(glib::markup_escape_text(message).as_str())
        .build();
    show_page(window, title, &page);
    show(window);
}
