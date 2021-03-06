use crate::cmd::Cmd;
use crate::color::Color;
use crate::widget::WaitContext;
use crate::widgets::bar_widget::{BarWidget, BarWidgetImpl};

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use nix::poll::{PollFd, PollFlags};

fn get_upower_property(
    con: &dbus::Connection,
    device_path: &str,
    property: &str,
) -> Result<dbus::Message, ::std::io::Error> {
    let msg = dbus::Message::new_method_call(
        "org.freedesktop.UPower",
        device_path,
        "org.freedesktop.DBus.Properties",
        "Get",
    )
    .map_err(|_| {
        ::std::io::Error::new(
            ::std::io::ErrorKind::Other,
            "could not send make dbus method call",
        )
    })?
    .append2(
        dbus::MessageItem::Str("org.freedesktop.UPower.Device".to_string()),
        dbus::MessageItem::Str(property.to_string()),
    );

    con.send_with_reply_and_block(msg, 1000).map_err(|_| {
        ::std::io::Error::new(::std::io::ErrorKind::Other, "could not send dbus message")
    })
}

// The widget is created on a different thread than where it will be used, so
// we need it to be Send. However, dbus::Connection has a void*, so it's not
// auto-derived. So, we make a small wrapper where we make it Send.
struct DbusConnection(dbus::Connection);

impl std::convert::AsRef<dbus::Connection> for DbusConnection {
    fn as_ref(&self) -> &dbus::Connection {
        &self.0
    }
}

unsafe impl Send for DbusConnection {}

pub struct UpowerBattery {
    device_path: String,
    sender: Sender<Cmd>,
    dirty: Arc<Mutex<bool>>,
    state: UpowerBatteryState,
    capacity: f64,
    con: DbusConnection,
    watch: dbus::Watch,
}

enum UpowerBatteryState {
    Charging,
    Discharging,
    Empty,
    Full,
    NotCharging,
    Unknown,
}

impl UpowerBattery {
    pub fn from_device(
        dirty: Arc<Mutex<bool>>,
        sender: Sender<Cmd>,
        device: &str,
    ) -> Result<Self, ::std::io::Error> {
        let con = dbus::Connection::get_private(dbus::BusType::System).map_err(|_| {
            ::std::io::Error::new(::std::io::ErrorKind::Other, "unable to open dbus")
        })?;
        let device_path = format!("/org/freedesktop/UPower/devices/battery_{}", device);

        let rule = format!(
            "type='signal',\
             path='{}',\
             interface='org.freedesktop.DBus.Properties',\
             member='PropertiesChanged'",
            &device_path
        );
        con.add_match(&rule).map_err(|_| {
            ::std::io::Error::new(
                ::std::io::ErrorKind::Other,
                "unable to add match rule to dbus connection",
            )
        })?;
        let upower_type: dbus::arg::Variant<u32> =
            match get_upower_property(&con, &device_path, "Type")?.get1() {
                Some(v) => v,
                None => {
                    return Err(::std::io::Error::new(
                        ::std::io::ErrorKind::Other,
                        "no such upower device",
                    ));
                }
            };

        // https://upower.freedesktop.org/docs/Device.html#Device:Type
        if upower_type.0 != 2 {
            return Err(::std::io::Error::new(
                ::std::io::ErrorKind::Other,
                "UPower device is not a battery.",
            ));
        }

        let capacity: f64 = match get_upower_property(&con, &device_path, "Percentage")?
            .get1::<dbus::arg::Variant<f64>>()
        {
            Some(v) => v.0,
            None => {
                return Err(::std::io::Error::new(
                    ::std::io::ErrorKind::Other,
                    "no such upower device",
                ));
            }
        };
        let state: UpowerBatteryState = match get_upower_property(&con, &device_path, "State")?
            .get1::<dbus::arg::Variant<u32>>()
        {
            Some(v) => match v.0 {
                1 => UpowerBatteryState::Charging,
                2 => UpowerBatteryState::Discharging,
                3 => UpowerBatteryState::Empty,
                4 => UpowerBatteryState::Full,
                5 => UpowerBatteryState::NotCharging,
                6 => UpowerBatteryState::Discharging,
                _ => UpowerBatteryState::Unknown,
            },
            None => {
                return Err(::std::io::Error::new(
                    ::std::io::ErrorKind::Other,
                    "no such upower device",
                ));
            }
        };

        let fds = con.watch_fds();
        if fds.len() != 1 {
            return Err(::std::io::Error::new(
                ::std::io::ErrorKind::Other,
                "expected 1 watch fd from dbus",
            ));
        }

        Ok(UpowerBattery {
            device_path,
            con: DbusConnection(con),
            dirty,
            sender,
            capacity,
            state,
            watch: fds[0],
        })
    }

    pub fn new(
        font_size: f32,
        length: u32,
        sender: Sender<Cmd>,
    ) -> Result<Box<BarWidget>, ::std::io::Error> {
        BarWidget::new(font_size, length, move |dirty| {
            let d = UpowerBattery::from_device(dirty, sender, "BAT0")?;
            Ok(Box::new(d))
        })
    }
}

impl BarWidgetImpl for UpowerBattery {
    fn wait(&mut self, ctx: &mut WaitContext) {
        for _ in self
            .con
            .as_ref()
            .watch_handle(self.watch.fd(), dbus::WatchEvent::Readable as u32)
        {
            let capacity = get_upower_property(self.con.as_ref(), &self.device_path, "Percentage")
                .unwrap()
                .get1::<dbus::arg::Variant<f64>>()
                .unwrap()
                .0;
            let state = match get_upower_property(self.con.as_ref(), &self.device_path, "State")
                .unwrap()
                .get1::<dbus::arg::Variant<u32>>()
                .unwrap()
                .0
            {
                1 => UpowerBatteryState::Charging,
                2 => UpowerBatteryState::Discharging,
                3 => UpowerBatteryState::Empty,
                4 => UpowerBatteryState::Full,
                5 => UpowerBatteryState::NotCharging,
                6 => UpowerBatteryState::Discharging,
                _ => UpowerBatteryState::Unknown,
            };
            self.state = state;
            self.capacity = capacity;
            *self.dirty.lock().unwrap() = true;
            self.sender.send(Cmd::Draw).unwrap();
        }

        ctx.fds
            .push(PollFd::new(self.watch.fd(), PollFlags::POLLIN));
    }
    fn name(&self) -> &str {
        "battery"
    }
    fn value(&self) -> f32 {
        (self.capacity as f32) / 100.0
    }
    fn color(&self) -> Color {
        match self.state {
            UpowerBatteryState::Discharging | UpowerBatteryState::Unknown => {
                if self.capacity > 10.0 {
                    Color::new(1.0, 1.0, 1.0, 1.0)
                } else {
                    Color::new(1.0, 0.5, 0.0, 1.0)
                }
            }
            UpowerBatteryState::Charging | UpowerBatteryState::Full => {
                Color::new(0.5, 1.0, 0.5, 1.0)
            }
            UpowerBatteryState::NotCharging | UpowerBatteryState::Empty => {
                Color::new(1.0, 0.5, 0.5, 1.0)
            }
        }
    }
    fn inc(&mut self, _: f32) {}
    fn set(&mut self, _: f32) {}
    fn toggle(&mut self) {}
}
