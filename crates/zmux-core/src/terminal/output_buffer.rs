//! Per-pane dirty line tracking (mirrors Zellij `OutputBuffer`).

use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub struct OutputBuffer {
    changed_lines: HashSet<usize>,
    should_update_all_lines: bool,
}

impl OutputBuffer {
    pub fn update_line(&mut self, line_index: usize) {
        if !self.should_update_all_lines {
            self.changed_lines.insert(line_index);
        }
    }

    pub fn update_all_lines(&mut self) {
        self.changed_lines.clear();
        self.should_update_all_lines = true;
    }

    pub fn clear(&mut self) {
        self.changed_lines.clear();
        self.should_update_all_lines = false;
    }

    pub fn needs_full_repaint(&self) -> bool {
        self.should_update_all_lines
    }

    pub fn changed_lines_in_viewport(
        &self,
        viewport_height: usize,
    ) -> Vec<usize> {
        if self.should_update_all_lines {
            (0..viewport_height).collect()
        } else {
            let mut lines: Vec<_> = self
                .changed_lines
                .iter()
                .copied()
                .filter(|i| *i < viewport_height)
                .collect();
            lines.sort_unstable();
            lines
        }
    }
}
