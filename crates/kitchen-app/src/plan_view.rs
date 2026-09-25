//! Plan reviewer. (stub)

use std::sync::Arc;

use crate::engine::{Engine, Tab, Workspace};
use crate::panels::{PanelCmd, PanelCtx};

pub struct PlanView {}

impl PlanView {
    pub fn new(_engine: &Arc<Engine>, _ws: &Workspace, _tab: &Tab) -> Self {
        Self {}
    }

    pub fn command(&mut self, _cmd: PanelCmd, _engine: &Arc<Engine>) {}

    pub fn ui(&mut self, ui: &mut egui::Ui, _cx: &mut PanelCtx) {
        ui.label("plan");
    }
}
