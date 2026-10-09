use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gst::prelude::*;
use gtk::{gdk, glib};

use crate::{decoders, resume, window};

const HIDE_DELAY: Duration = Duration::from_secs(2);
/// How often the position is saved during playback, so a crash loses little.
const SAVE_INTERVAL: Duration = Duration::from_secs(15);
const TICK: Duration = Duration::from_millis(250);
/// While the user drags the seek bar, the playback clock lags behind; don't
/// let the periodic update snap the slider back during this window.
const USER_SEEK_GRACE: Duration = Duration::from_millis(500);
/// A seek that hasn't landed by then is treated as lost, so later seeks
/// aren't queued forever.
const SEEK_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest share of the screen a freshly opened video's window may take.
const MAX_SCREEN_SHARE: f64 = 0.7;
const SEEK_STEP: f64 = 10.0;
const VOLUME_STEP: f64 = 0.05;

pub fn build_pipeline(uri: &str) -> Result<(gst::Element, gdk::Paintable), glib::BoolError> {
    let gtksink = gst::ElementFactory::make("gtk4paintablesink").build()?;
    let paintable = gtksink.property::<gdk::Paintable>("paintable");

    let video_sink = if paintable
        .property::<Option<gdk::GLContext>>("gl-context")
        .is_some()
    {
        gst::ElementFactory::make("glsinkbin")
            .property("sink", &gtksink)
            .build()?
    } else {
        gtksink
    };

    let playbin = gst::ElementFactory::make("playbin3")
        .property("uri", uri)
        .property("video-sink", &video_sink)
        .build()?;

    Ok((playbin, paintable))
}

pub struct Player {
    this: Weak<Player>,
    title: String,
    uri: String,
    window: adw::ApplicationWindow,
    playbin: gst::Element,
    toasts: adw::ToastOverlay,
    overlay: gtk::Overlay,
    header_revealer: gtk::Revealer,
    controls_revealer: gtk::Revealer,
    play_button: gtk::Button,
    seek_scale: gtk::Scale,
    position_label: gtk::Label,
    duration_label: gtk::Label,
    mute_button: gtk::Button,
    volume_scale: gtk::Scale,
    fullscreen_button: gtk::Button,
    /// Reopens the same file at a position, after hardware decoding failed.
    reload: Box<dyn Fn(f64)>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    playing: bool,
    muted: bool,
    ended: bool,
    fitted: bool,
    /// Saved position to jump to once the file has loaded.
    resume_at: Option<f64>,
    /// Whether that jump came from the saved positions and should be shown.
    announce_resume: bool,
    reloading: bool,
    last_saved: Option<Instant>,
    duration: Option<gst::ClockTime>,
    /// When the user last seeked, and to where (seconds).
    last_user_seek: Option<Instant>,
    seek_target: f64,
    seek_in_flight: Option<Instant>,
    queued_seek: Option<f64>,
    pointer: Option<(f64, f64)>,
    over_controls: bool,
    hide_source: Option<glib::SourceId>,
    tick_source: Option<glib::SourceId>,
    bus_watch: Option<gst::bus::BusWatchGuard>,
    fullscreen_handler: Option<glib::SignalHandlerId>,
    breakpoint_handler: Option<glib::SignalHandlerId>,
    key_controller: Option<gtk::EventControllerKey>,
}

impl Player {
    pub fn new(
        window: &adw::ApplicationWindow,
        title: &str,
        uri: &str,
        playbin: gst::Element,
        paintable: &gdk::Paintable,
        reload: impl Fn(f64) + 'static,
    ) -> Rc<Self> {
        let picture = gtk::Picture::builder()
            .paintable(paintable)
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();
        picture.add_css_class("monet-video");

        let header = window::header_bar(title);
        header.add_css_class("monet-header");
        let header_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .reveal_child(true)
            .valign(gtk::Align::Start)
            .child(&header)
            .build();

        let play_button = icon_button("media-playback-start-symbolic", "Play");
        let position_label = time_label();
        let seek_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        seek_scale.set_hexpand(true);
        seek_scale.set_sensitive(false);
        let duration_label = time_label();
        let mute_button = icon_button("audio-volume-high-symbolic", "Mute");
        let volume_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        volume_scale.set_value(1.0);
        volume_scale.set_width_request(100);
        let fullscreen_button = icon_button("view-fullscreen-symbolic", "Fullscreen");

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.add_css_class("osd");
        controls.add_css_class("monet-controls");
        controls.set_margin_start(12);
        controls.set_margin_end(12);
        controls.set_margin_bottom(12);
        controls.append(&play_button);
        controls.append(&position_label);
        controls.append(&seek_scale);
        controls.append(&duration_label);
        controls.append(&mute_button);
        controls.append(&volume_scale);
        controls.append(&fullscreen_button);

        let controls_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .reveal_child(true)
            .valign(gtk::Align::End)
            .child(&controls)
            .build();

        let overlay = gtk::Overlay::builder().child(&picture).build();
        overlay.add_overlay(&header_revealer);
        overlay.add_overlay(&controls_revealer);

        let toasts = adw::ToastOverlay::new();
        toasts.add_css_class("monet-toasts");
        toasts.set_child(Some(&overlay));
        window.set_content(Some(&toasts));

        let player = Rc::new_cyclic(|this| Self {
            this: this.clone(),
            title: title.to_owned(),
            uri: uri.to_owned(),
            window: window.clone(),
            playbin,
            toasts,
            overlay,
            header_revealer,
            controls_revealer,
            play_button,
            seek_scale,
            position_label,
            duration_label,
            mute_button,
            volume_scale,
            fullscreen_button,
            reload: Box::new(reload),
            state: RefCell::default(),
        });
        player.connect_signals(&picture, &controls);
        let this = player.this.clone();
        paintable.connect_invalidate_size(move |paintable| {
            if let Some(player) = this.upgrade() {
                player.fit_to_video(paintable);
            }
        });
        player
    }

    /// Sizes the window to the video's shape once its first frame is known,
    /// then shows it if it was waiting for that.
    fn fit_to_video(&self, paintable: &gdk::Paintable) {
        let (width, height) = (
            paintable.intrinsic_width() as f64,
            paintable.intrinsic_height() as f64,
        );
        if width <= 0.0 || height <= 0.0 || self.state.borrow().fitted {
            return;
        }
        self.state.borrow_mut().fitted = true;

        if !self.window.is_maximized() && !self.window.is_fullscreen() {
            let display = WidgetExt::display(&self.window);
            let monitor = self
                .window
                .surface()
                .and_then(|surface| display.monitor_at_surface(&surface))
                .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>());
            if let Some(monitor) = monitor {
                let area = monitor.geometry();
                let scale = (area.width() as f64 * MAX_SCREEN_SHARE / width)
                    .min(area.height() as f64 * MAX_SCREEN_SHARE / height)
                    .min(1.0);
                let (min_width, min_height) = window::MIN_SIZE;
                // Small videos grow just enough to clear the minimum size,
                // keeping their shape.
                let scale = scale
                    .max(min_width as f64 / width)
                    .max(min_height as f64 / height);
                self.window.set_default_size(
                    (width * scale).round() as i32,
                    (height * scale).round() as i32,
                );
            }
        }
        window::show(&self.window);
    }

    /// Runs `f` with the player if it is still alive. Signal handlers hold
    /// only weak references so widgets never keep the player alive.
    fn with(&self, f: impl Fn(&Player) + 'static) -> impl Fn() + 'static {
        let this = self.this.clone();
        move || {
            if let Some(player) = this.upgrade() {
                f(&player);
            }
        }
    }

    fn connect_signals(self: &Rc<Self>, picture: &gtk::Picture, controls: &gtk::Box) {
        let toggle_play = self.with(Player::toggle_play);
        self.play_button.connect_clicked(move |_| toggle_play());

        let this = self.this.clone();
        self.seek_scale.connect_change_value(move |scale, _, value| {
            if let Some(player) = this.upgrade() {                let value = value.clamp(0.0, scale.adjustment().upper());
                {
                    let mut state = player.state.borrow_mut();
                    state.last_user_seek = Some(Instant::now());
                    state.seek_target = value;
                }
                player.position_label.set_label(&format_time(seconds(value)));
                player.seek_to(value);
            }
            glib::Propagation::Proceed
        });

        let toggle_mute = self.with(Player::toggle_mute);
        self.mute_button.connect_clicked(move |_| toggle_mute());

        let volume_changed = self.with(Player::volume_changed);
        self.volume_scale.connect_value_changed(move |_| volume_changed());

        let toggle_fullscreen = self.with(Player::toggle_fullscreen);
        self.fullscreen_button.connect_clicked(move |_| toggle_fullscreen());

        let fullscreen_changed = self.with(Player::fullscreen_changed);
        let handler = self
            .window
            .connect_fullscreened_notify(move |_| fullscreen_changed());
        self.state.borrow_mut().fullscreen_handler = Some(handler);
        self.fullscreen_changed();

        let breakpoint_changed = self.with(Player::breakpoint_changed);
        let handler = self
            .window
            .connect_current_breakpoint_notify(move |_| breakpoint_changed());
        self.state.borrow_mut().breakpoint_handler = Some(handler);
        self.breakpoint_changed();

        // Capture phase: shortcuts win over the focused widget, so Space
        // doesn't also press a focused button and arrows don't nudge sliders.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = self.this.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let handled = !modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) && this.upgrade().is_some_and(|player| player.handle_key(key));
            if handled {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(keys.clone());
        self.state.borrow_mut().key_controller = Some(keys);

        let double_click = gtk::GestureClick::new();
        let toggle_fullscreen = self.with(Player::toggle_fullscreen);
        double_click.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                toggle_fullscreen();
            }
        });
        picture.add_controller(double_click);

        let motion = gtk::EventControllerMotion::new();
        let this = self.this.clone();
        motion.connect_motion(move |_, x, y| {
            if let Some(player) = this.upgrade() {
                player.pointer_moved(x, y);
            }
        });
        let hide_controls = self.with(Player::hide_controls);
        motion.connect_leave(move |_| hide_controls());
        self.overlay.add_controller(motion);

        let controls_motion = gtk::EventControllerMotion::new();
        let this = self.this.clone();
        controls_motion.connect_enter(move |_, _, _| {
            if let Some(player) = this.upgrade() {
                player.state.borrow_mut().over_controls = true;
            }
        });
        let this = self.this.clone();
        controls_motion.connect_leave(move |_| {
            if let Some(player) = this.upgrade() {
                player.state.borrow_mut().over_controls = false;
                player.schedule_hide();
            }
        });
        controls.add_controller(controls_motion);

        let bus = self.playbin.bus().expect("playbin always has a bus");
        let this = self.this.clone();
        let watch = bus
            .add_watch_local(move |_, msg| match this.upgrade() {
                Some(player) => {
                    player.handle_message(msg);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            })
            .expect("bus has no other watch");
        self.state.borrow_mut().bus_watch = Some(watch);

        let this = self.this.clone();
        let tick = glib::timeout_add_local(TICK, move || match this.upgrade() {
            Some(player) => {
                player.update_position();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        self.state.borrow_mut().tick_source = Some(tick);
    }

    /// Playback starts asynchronously: GStreamer prerolls on its own
    /// threads, so the window stays responsive while the file loads.
    /// Starts at `start_at` if given, else at the saved position, if any.
    pub fn start(&self, start_at: Option<f64>) {
        self.state.borrow_mut().last_saved = Some(Instant::now());
        let announce = start_at.is_none();
        let resume_at = start_at.or_else(|| resume::load(&self.uri));
        if resume_at.is_none() {
            self.set_playing(true);
            return;
        }
        // Load paused, jump to the saved spot, then play; this avoids a
        // flash of the opening frames before the seek.
        {
            let mut state = self.state.borrow_mut();
            state.resume_at = resume_at;
            state.announce_resume = announce;
        }
        if let Err(err) = self.playbin.set_state(gst::State::Paused) {
            self.fail(&err.to_string());
        }
    }

    fn resume_from(&self, position: f64) {
        let announce = self.state.borrow().announce_resume;
        let duration = self.playbin.query_duration::<gst::ClockTime>().map(secs);
        if !announce || duration.is_none_or(|duration| position < duration - resume::END_MARGIN) {
            {
                let mut state = self.state.borrow_mut();
                state.last_user_seek = Some(Instant::now());
                state.seek_target = position;
            }
            self.seek_scale.set_value(position);
            self.position_label.set_label(&format_time(seconds(position)));
            self.seek_to(position);
            if announce {
                self.show_resume_toast(position);
            }
        }
        self.set_playing(true);
    }

    fn show_resume_toast(&self, position: f64) {
        let toast = adw::Toast::builder()
            .title(format!("Resumed from {}", format_time(seconds(position))))
            .button_label("Start Over")
            .timeout(5)
            .build();
        let start_over = self.with(|player| {
            player.seek_to(0.0);
            player.show_controls();
        });
        toast.connect_button_clicked(move |_| start_over());
        self.toasts.add_toast(toast);
    }

    fn save_position(&self) {
        let (ended, duration) = {
            let state = self.state.borrow();
            if state.resume_at.is_some() {
                return;
            }
            (state.ended, state.duration.map(secs))
        };
        let position = self.current_position();
        if ended {
            resume::forget(&self.uri);
        } else if let Some(position) = position {
            resume::save(&self.uri, position, duration);
        }
        self.state.borrow_mut().last_saved = Some(Instant::now());
    }

    /// Where playback is, or is about to be once a pending seek lands.
    fn current_position(&self) -> Option<f64> {
        if self.seek_pending() {
            Some(self.state.borrow().seek_target)
        } else {
            self.playbin.query_position::<gst::ClockTime>().map(secs)
        }
    }

    fn set_playing(&self, playing: bool) {
        if playing && self.state.borrow().ended {
            self.seek_to(0.0);
        }
        self.state.borrow_mut().playing = playing;

        let target = if playing { gst::State::Playing } else { gst::State::Paused };
        if let Err(err) = self.playbin.set_state(target) {
            self.fail(&err.to_string());
            return;
        }

        let (icon, tooltip) = if playing {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        self.play_button.set_icon_name(icon);
        self.play_button.set_tooltip_text(Some(tooltip));
        self.show_controls();
        if !playing {
            self.save_position();
        }
    }

    fn toggle_play(&self) {
        let playing = self.state.borrow().playing;
        self.set_playing(!playing);
    }

    /// Seeks to the exact time. Accurate seeks decode from the previous
    /// keyframe, so while one is in flight newer targets are queued and only
    /// the latest runs once it lands; dragging stays smooth without piling up.
    fn seek_to(&self, secs: f64) {
        {
            let mut state = self.state.borrow_mut();
            state.ended = false;
            state.seek_target = secs;
            let busy = state
                .seek_in_flight
                .is_some_and(|at| at.elapsed() < SEEK_TIMEOUT);
            if busy {
                state.queued_seek = Some(secs);
                return;
            }
            state.seek_in_flight = Some(Instant::now());
        }
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        if self.playbin.seek_simple(flags, seconds(secs)).is_err() {
            self.state.borrow_mut().seek_in_flight = None;
        }
    }

    /// True while a user seek hasn't landed yet, during which the playback
    /// clock still reports the old position.
    fn seek_pending(&self) -> bool {
        let state = self.state.borrow();
        state.seek_in_flight.is_some_and(|at| at.elapsed() < SEEK_TIMEOUT)
            || state.queued_seek.is_some()
            || state
                .last_user_seek
                .is_some_and(|at| at.elapsed() < USER_SEEK_GRACE)
    }

    fn seek_finished(&self) {
        let queued = {
            let mut state = self.state.borrow_mut();
            state.seek_in_flight = None;
            state.queued_seek.take()
        };
        if let Some(target) = queued {
            self.seek_to(target);
        }
    }

    fn seek_by(&self, delta: f64) {
        // A seek in flight hasn't moved the clock yet; build on its target so
        // rapid or held key presses accumulate instead of repeating.
        let pending = self.seek_pending().then(|| self.state.borrow().seek_target);
        let Some(base) = pending.or_else(|| {
            self.playbin
                .query_position::<gst::ClockTime>()
                .map(secs)
        }) else {
            return;
        };
        let mut target = (base + delta).max(0.0);
        if let Some(duration) = self.state.borrow().duration {
            target = target.min(secs(duration));
        }
        {
            let mut state = self.state.borrow_mut();
            state.last_user_seek = Some(Instant::now());
            state.seek_target = target;
        }
        self.seek_scale.set_value(target);
        self.position_label.set_label(&format_time(seconds(target)));
        self.seek_to(target);
        self.show_controls();
    }

    fn change_volume(&self, delta: f64) {
        let value = (self.volume_scale.value() + delta).clamp(0.0, 1.0);
        self.volume_scale.set_value(value);
        if delta > 0.0 {
            self.set_muted(false);
        }
        self.show_controls();
    }

    fn handle_key(&self, key: gdk::Key) -> bool {        match key.to_lower() {
            gdk::Key::space => self.toggle_play(),
            gdk::Key::Left => self.seek_by(-SEEK_STEP),
            gdk::Key::Right => self.seek_by(SEEK_STEP),
            gdk::Key::Up => self.change_volume(VOLUME_STEP),
            gdk::Key::Down => self.change_volume(-VOLUME_STEP),
            gdk::Key::f => self.toggle_fullscreen(),
            gdk::Key::m => self.toggle_mute(),
            gdk::Key::Escape if self.window.is_fullscreen() => self.window.unfullscreen(),
            _ => return false,
        }
        true
    }

    fn detach_keys(&self) {
        let keys = self.state.borrow_mut().key_controller.take();
        if let Some(keys) = keys {
            self.window.remove_controller(&keys);
        }
    }

    fn toggle_mute(&self) {
        let muted = self.state.borrow().muted;
        self.set_muted(!muted);
    }

    /// Our own flag is the source of truth: the sink's "mute" property is
    /// synced with the sound server asynchronously and can read back stale.
    fn set_muted(&self, muted: bool) {
        self.state.borrow_mut().muted = muted;
        self.playbin.set_property("mute", muted);
        self.update_volume_icon();
    }

    fn volume_changed(&self) {
        let value = self.volume_scale.value();
        // Loudness is perceived roughly cubically; this makes the slider feel even.
        self.playbin.set_property("volume", value.powi(3));
        if value > 0.0 {
            self.set_muted(false);
        } else {
            self.update_volume_icon();
        }
    }

    fn update_volume_icon(&self) {
        let muted = self.state.borrow().muted;
        let value = self.volume_scale.value();
        let icon = if muted || value == 0.0 {
            "audio-volume-muted-symbolic"
        } else if value < 0.34 {
            "audio-volume-low-symbolic"
        } else if value < 0.67 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        self.mute_button.set_icon_name(icon);
        self.mute_button
            .set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
    }

    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }

    fn fullscreen_changed(&self) {
        let (icon, tooltip) = if self.window.is_fullscreen() {
            ("view-restore-symbolic", "Leave Fullscreen")
        } else {
            ("view-fullscreen-symbolic", "Fullscreen")
        };
        self.fullscreen_button.set_icon_name(icon);
        self.fullscreen_button.set_tooltip_text(Some(tooltip));
    }

    fn breakpoint_changed(&self) {
        let narrow = self.window.current_breakpoint().is_some();
        self.volume_scale.set_visible(!narrow);    }

    fn pointer_moved(&self, x: f64, y: f64) {
        // GTK also reports "motion" when widgets change under a still pointer.
        let moved = self.state.borrow().pointer != Some((x, y));
        if moved {
            self.state.borrow_mut().pointer = Some((x, y));
            self.show_controls();
        }
    }

    fn show_controls(&self) {
        self.header_revealer.set_reveal_child(true);
        self.controls_revealer.set_reveal_child(true);
        self.overlay.set_cursor(None);
        self.schedule_hide();
    }

    fn schedule_hide(&self) {
        let mut state = self.state.borrow_mut();
        if let Some(source) = state.hide_source.take() {
            source.remove();
        }
        let this = self.this.clone();
        state.hide_source = Some(glib::timeout_add_local_once(HIDE_DELAY, move || {
            if let Some(player) = this.upgrade() {
                player.state.borrow_mut().hide_source = None;
                player.hide_controls();
            }
        }));
    }

    fn hide_controls(&self) {
        {
            let state = self.state.borrow();
            if !state.playing || state.over_controls {
                return;
            }
        }
        self.header_revealer.set_reveal_child(false);
        self.controls_revealer.set_reveal_child(false);
        self.overlay.set_cursor_from_name(Some("none"));
    }

    fn update_position(&self) {
        let known = self.state.borrow().duration.is_some();
        if !known && let Some(duration) = self.playbin.query_duration::<gst::ClockTime>() {
            self.state.borrow_mut().duration = Some(duration);
            self.seek_scale.set_range(0.0, secs(duration).max(1.0));
            self.seek_scale.set_sensitive(true);
            self.duration_label.set_label(&format_time(duration));
        }

        // Mid-seek the clock still reports the old spot; showing it would
        // make the slider jump back before landing.
        if !self.seek_pending()
            && let Some(position) = self.playbin.query_position::<gst::ClockTime>()
        {
            self.seek_scale.set_value(secs(position));
            self.position_label.set_label(&format_time(position));
        }

        let save_due = {
            let state = self.state.borrow();
            state.playing && state.last_saved.is_none_or(|at| at.elapsed() >= SAVE_INTERVAL)
        };
        if save_due {
            self.save_position();
        }
    }

    fn handle_message(&self, msg: &gst::Message) {
        match msg.view() {
            gst::MessageView::Error(err) => {
                match err.debug() {
                    Some(debug) => eprintln!("monet: {}\n{debug}", err.error()),
                    None => eprintln!("monet: {}", err.error()),
                }
                if self.state.borrow().reloading {
                    return;
                }
                if decoders::hardware_in_use(&self.playbin) && decoders::disable_hardware() {
                    eprintln!("monet: hardware decoding failed, retrying with software");
                    self.state.borrow_mut().reloading = true;
                    let position = self
                        .current_position()
                        .or(self.state.borrow().resume_at)
                        .unwrap_or(0.0);
                    // Reloading replaces this player; do it outside its own bus callback.
                    let this = self.this.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(player) = this.upgrade() {
                            (player.reload)(position);
                        }
                    });
                    return;
                }
                self.fail(&err.error().to_string());
            }
            gst::MessageView::Eos(_) => {
                self.state.borrow_mut().ended = true;
                self.set_playing(false);
            }
            gst::MessageView::DurationChanged(_) => {
                self.state.borrow_mut().duration = None;
            }
            gst::MessageView::AsyncDone(_) => {
                let resume_at = self.state.borrow_mut().resume_at.take();
                match resume_at {
                    Some(position) => self.resume_from(position),
                    None => self.seek_finished(),
                }
            }
            _ => {}
        }
    }

    fn fail(&self, message: &str) {
        let _ = self.playbin.set_state(gst::State::Null);
        if let Some(tick) = self.state.borrow_mut().tick_source.take() {
            tick.remove();
        }
        self.detach_keys();
        window::show_error(&self.window, &self.title, message);
    }

    /// Stops playback and detaches from the window. Called when the window
    /// closes or another file replaces this one.
    pub fn shutdown(&self) {
        self.save_position();
        let mut state = self.state.borrow_mut();
        if let Some(tick) = state.tick_source.take() {
            tick.remove();
        }
        if let Some(hide) = state.hide_source.take() {
            hide.remove();
        }
        for handler in [state.fullscreen_handler.take(), state.breakpoint_handler.take()]
            .into_iter()
            .flatten()
        {
            self.window.disconnect(handler);
        }
        state.bus_watch.take();
        drop(state);
        self.detach_keys();
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build();
    button.add_css_class("flat");
    button.add_css_class("circular");
    button
}

fn time_label() -> gtk::Label {
    let label = gtk::Label::new(Some("0:00"));
    label.add_css_class("numeric");
    label
}

fn seconds(secs: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64)
}

fn secs(time: gst::ClockTime) -> f64 {
    time.nseconds() as f64 / 1e9
}

fn format_time(time: gst::ClockTime) -> String {
    let total = time.seconds();
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
