//! Review inbox: agent-drafted comments waiting for approval. (stub)

use std::sync::Arc;

use crate::engine::Engine;
use crate::panels::{PanelCmd, PanelCtx};

pub struct Inbox {}

impl Inbox {
    pub fn new(_engine: &Arc<Engine>) -> Self {
        Self {}
    }

    pub fn pending_count(&self) -> usize {
        0
    }

    pub fn command(&mut self, _cmd: PanelCmd, _engine: &Arc<Engine>) {}

    pub fn ui(&mut self, ui: &mut egui::Ui, _cx: &mut PanelCtx) {
        ui.label("inbox");
    }
}
