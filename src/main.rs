use ksni::blocking::TrayMethods;
use ksni::Tray;

mod config;
mod creds;
mod events;
mod pricing;
mod quota;
mod session;
mod state;

struct Probe;

impl Tray for Probe {
    fn id(&self) -> String {
        "claude-usage".into()
    }

    fn title(&self) -> String {
        "claude-usage".into()
    }

    fn icon_name(&self) -> String {
        "utilities-system-monitor-symbolic".into()
    }

    fn label(&self) -> String {
        "98%".into()
    }
}

fn main() {
    match Probe.spawn() {
        Ok(_handle) => loop {
            std::thread::park();
        },
        Err(err) => eprintln!("failed to start tray service: {err}"),
    }
}
