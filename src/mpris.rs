//! MPRIS: lets GNOME's media controls, media keys and headset buttons
//! drive the player. https://specifications.freedesktop.org/mpris-spec/latest/

use std::cell::RefCell;
use std::rc::Weak;
use std::sync::atomic::{AtomicU32, Ordering};

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::player::Player;

const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const TRACK: &str = "/io/github/rajnish/Monet/Track";
const XML: &str = r#"
<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="Identity" type="s" access="read"/>
    <property name="DesktopEntry" type="s" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <method name="Seek"><arg direction="in" name="Offset" type="x"/></method>
    <method name="SetPosition">
      <arg direction="in" name="TrackId" type="o"/>
      <arg direction="in" name="Position" type="x"/>
    </method>
    <method name="OpenUri"><arg direction="in" name="Uri" type="s"/></method>
    <signal name="Seeked"><arg name="Position" type="x"/></signal>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="LoopStatus" type="s" access="readwrite"/>
    <property name="Rate" type="d" access="readwrite"/>
    <property name="Shuffle" type="b" access="readwrite"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Volume" type="d" access="readwrite"/>
    <property name="Position" type="x" access="read"/>
    <property name="MinimumRate" type="d" access="read"/>
    <property name="MaximumRate" type="d" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>"#;

/// Each window gets its own name, since one process can play several videos.
static INSTANCE: AtomicU32 = AtomicU32::new(0);

pub struct Mpris {
    connection: gio::DBusConnection,
    registrations: RefCell<Vec<gio::RegistrationId>>,
    owner: RefCell<Option<gio::OwnerId>>,
}

impl Mpris {
    pub fn new(player: Weak<Player>) -> Option<Self> {
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
        let node = gio::DBusNodeInfo::for_xml(XML).ok()?;
        let mut registrations = Vec::new();
        for name in [ROOT, PLAYER] {
            let interface = node.lookup_interface(name)?;
            let (get, set, call) = (player.clone(), player.clone(), player.clone());
            let id = connection
                .register_object(PATH, &interface)
                .property(move |_, _, _, interface, property| {
                    get.upgrade()
                        .and_then(|player| property_value(&player, interface, property))
                        .unwrap_or_else(|| false.to_variant())
                })
                .set_property(move |_, _, _, _, property, value| {
                    set.upgrade()
                        .is_some_and(|player| set_property(&player, property, &value))
                })
                .method_call(move |_, _, _, _, method, parameters, invocation| {
                    match call.upgrade() {
                        Some(player) => handle_call(&player, method, &parameters, invocation),
                        None => invocation.return_value(None),
                    }
                })
                .build()
                .ok()?;
            registrations.push(id);
        }
        let name = format!(
            "{ROOT}.monet.instance{}_{}",
            std::process::id(),
            INSTANCE.fetch_add(1, Ordering::Relaxed)
        );
        let owner = gio::bus_own_name_on_connection(
            &connection,
            &name,
            gio::BusNameOwnerFlags::NONE,
            |_, _| {},
            |_, _| {},
        );
        Some(Self {
            connection,
            registrations: RefCell::new(registrations),
            owner: RefCell::new(Some(owner)),
        })
    }

    /// Tells listeners these player properties changed.
    pub fn changed(&self, player: &Player, properties: &[&str]) {
        let changed = glib::VariantDict::new(None);
        for &property in properties {
            if let Some(value) = property_value(player, PLAYER, property) {
                changed.insert_value(property, &value);
            }
        }
        let body = glib::Variant::tuple_from_iter([
            PLAYER.to_variant(),
            changed.end(),
            Vec::<String>::new().to_variant(),
        ]);
        let _ = self.connection.emit_signal(
            None,
            PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            Some(&body),
        );
    }

    /// Tells listeners playback jumped, so they don't keep extrapolating.
    pub fn seeked(&self, seconds: f64) {
        let _ = self
            .connection
            .emit_signal(None, PATH, PLAYER, "Seeked", Some(&(micros(seconds),).to_variant()));
    }

    pub fn close(&self) {
        if let Some(owner) = self.owner.borrow_mut().take() {
            gio::bus_unown_name(owner);
        }
        for id in self.registrations.borrow_mut().drain(..) {
            let _ = self.connection.unregister_object(id);
        }
    }
}

fn property_value(player: &Player, interface: &str, property: &str) -> Option<glib::Variant> {
    let value = match (interface, property) {
        (ROOT, "CanQuit" | "CanRaise") => true.to_variant(),
        (ROOT, "HasTrackList") => false.to_variant(),
        (ROOT, "Identity") => "Monet".to_variant(),
        (ROOT, "DesktopEntry") => crate::APP_ID.to_variant(),
        (ROOT, "SupportedUriSchemes" | "SupportedMimeTypes") => Vec::<String>::new().to_variant(),
        (PLAYER, "PlaybackStatus") => if player.playing() { "Playing" } else { "Paused" }.to_variant(),
        (PLAYER, "LoopStatus") => if player.looping() { "Track" } else { "None" }.to_variant(),
        (PLAYER, "Rate" | "MinimumRate" | "MaximumRate") => 1.0.to_variant(),
        (PLAYER, "Shuffle" | "CanGoNext" | "CanGoPrevious") => false.to_variant(),
        (PLAYER, "Metadata") => metadata(player),
        (PLAYER, "Volume") => player.volume().to_variant(),
        (PLAYER, "Position") => micros(player.current_position().unwrap_or(0.0)).to_variant(),
        (PLAYER, "CanPlay" | "CanPause" | "CanSeek" | "CanControl") => true.to_variant(),
        _ => return None,
    };
    Some(value)
}

fn metadata(player: &Player) -> glib::Variant {
    let metadata = glib::VariantDict::new(None);
    if let Ok(track) = glib::variant::ObjectPath::try_from(TRACK.to_owned()) {
        metadata.insert_value("mpris:trackid", &track.to_variant());
    }
    let title = player.title();
    let title = std::path::Path::new(title)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(title);
    metadata.insert_value("xesam:title", &title.to_variant());
    metadata.insert_value("xesam:url", &player.uri().to_variant());
    if let Some(duration) = player.duration() {
        metadata.insert_value("mpris:length", &micros(duration).to_variant());
    }
    metadata.end()
}

fn set_property(player: &Player, property: &str, value: &glib::Variant) -> bool {
    match property {
        "LoopStatus" => {
            if let Some(status) = value.str() {
                player.set_looping(status != "None");
            }
        }
        "Volume" => {
            if let Some(volume) = value.get::<f64>() {
                player.set_volume(volume.clamp(0.0, 1.0));
            }
        }
        // One speed and no playlist; accepted and ignored, as the spec allows.
        _ => {}
    }
    true
}

fn handle_call(
    player: &Player,
    method: &str,
    parameters: &glib::Variant,
    invocation: gio::DBusMethodInvocation,
) {
    match method {
        "Raise" => player.raise(),
        "Quit" => player.quit(),
        "Play" => player.set_playing(true),
        "Pause" | "Stop" => player.set_playing(false),
        "PlayPause" => player.toggle_play(),
        "Seek" => {
            if let Some((offset,)) = parameters.get::<(i64,)>() {
                player.seek_by(offset as f64 / 1e6);
            }
        }
        "SetPosition" => {
            if let Some(position) = parameters.try_child_value(1).and_then(|v| v.get::<i64>()) {
                player.jump_to(position as f64 / 1e6);
            }
        }
        "OpenUri" => {
            invocation.return_dbus_error(
                "org.freedesktop.DBus.Error.NotSupported",
                "Open files from the Monet window",
            );
            return;
        }
        // Next and Previous: there's nothing to go to.
        _ => {}
    }
    invocation.return_value(None);
}

fn micros(seconds: f64) -> i64 {
    (seconds * 1e6) as i64
}
