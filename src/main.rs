use ksni::{Tray, TrayService};

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
}

fn main() {
    TrayService::new(Probe).run().ok();
}
