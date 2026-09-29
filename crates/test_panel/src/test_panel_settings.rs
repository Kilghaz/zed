use gpui::Pixels;
pub use settings::DockPosition;
use settings::{IntoGpui, RegisterSetting, Settings};

#[derive(Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct TestPanelSettings {
    pub button: bool,
    pub default_width: Pixels,
    pub default_height: Pixels,
    pub dock: DockPosition,
}

impl Settings for TestPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.test_panel.as_ref().unwrap();
        Self {
            button: panel.button.unwrap(),
            default_width: panel.default_width.unwrap().into_gpui(),
            default_height: panel.default_height.unwrap().into_gpui(),
            dock: panel.dock.unwrap(),
        }
    }
}
