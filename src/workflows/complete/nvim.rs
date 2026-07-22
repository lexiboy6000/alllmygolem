//! A minimal, authoritative neovim model.
//!
//! It is the single source of truth for the subset of vim the "complete task"
//! workflow drives, and it powers the pre-typing SELF-TEST: before any real
//! keystrokes are sent, the whole generated [`Action`] stream is replayed
//! through this model (with no delays) and the resulting buffer is compared to
//! the netlist. If they differ, the workflow aborts instead of typing garbage.
//!
//! The browser-side `NVIM_DEMO_HTML` simulator mirrors this exactly (and the
//! end-to-end test verifies that), so the same action stream reconstructs the
//! text in the demo, in this model, and on a real neovim that supports the same
//! standard motions (`:N<CR>`, `0`, `{count}l`, `r`, `G`, `$`, `a`).
//!
//! Cursor model (matches the demo): `col` is the INSERT-mode insertion index
//! `0..=len`; on `<Esc>` vim moves left one (`if col>0 col--`), so NORMAL-mode
//! `col` sits ON a character `0..=len-1`.

use super::typing::{Action, Event};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
    Command,
}

struct Nvim {
    mode: Mode,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    cmd: String,
    count: usize,
    count_active: bool,
    pending_replace: bool,
    saves: usize,
}

impl Nvim {
    fn new() -> Self {
        Nvim {
            mode: Mode::Normal,
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            cmd: String::new(),
            count: 0,
            count_active: false,
            pending_replace: false,
            saves: 0,
        }
    }

    fn line_len(&self) -> usize {
        self.lines.get(self.row).map_or(0, |l| l.len())
    }

    fn reset_count(&mut self) {
        self.count = 0;
        self.count_active = false;
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::EnterInsert => self.mode = Mode::Insert,
            Action::Escape => {
                self.mode = Mode::Normal;
                self.cmd.clear();
                self.pending_replace = false;
                self.reset_count();
                if self.col > 0 {
                    self.col -= 1;
                }
            }
            Action::Char(c) => {
                if self.mode == Mode::Insert
                    && let Some(line) = self.lines.get_mut(self.row)
                {
                    let p = self.col.min(line.len());
                    line.insert(p, c);
                    self.col = p + 1;
                }
            }
            Action::Enter => {
                if self.mode == Mode::Insert
                    && let Some(line) = self.lines.get_mut(self.row)
                {
                    let p = self.col.min(line.len());
                    let tail = line.split_off(p);
                    self.lines.insert(self.row + 1, tail);
                    self.row += 1;
                    self.col = 0;
                }
            }
            Action::Backspace => {
                if self.mode == Mode::Insert {
                    if self.col > 0 {
                        if let Some(line) = self.lines.get_mut(self.row) {
                            let idx = self.col - 1;
                            if idx < line.len() {
                                line.remove(idx);
                            }
                            self.col -= 1;
                        }
                    } else if self.row > 0 {
                        let cur = self.lines.remove(self.row);
                        self.row -= 1;
                        if let Some(prev) = self.lines.get_mut(self.row) {
                            self.col = prev.len();
                            prev.extend(cur);
                        }
                    }
                }
            }
            Action::CmdEnter => {
                if self.mode == Mode::Command {
                    self.run_command();
                }
                self.mode = Mode::Normal;
                self.cmd.clear();
            }
            Action::Key(c) => self.key(c),
        }
    }

    fn key(&mut self, c: char) {
        match self.mode {
            Mode::Command => self.cmd.push(c),
            Mode::Insert => {
                // Not expected (the generator only emits Key in normal/command);
                // treat defensively as a literal insert so the model never panics.
                if let Some(line) = self.lines.get_mut(self.row) {
                    let p = self.col.min(line.len());
                    line.insert(p, c);
                    self.col = p + 1;
                }
            }
            Mode::Normal => self.normal_key(c),
        }
    }

    fn normal_key(&mut self, c: char) {
        if self.pending_replace {
            if let Some(line) = self.lines.get_mut(self.row)
                && let Some(slot) = line.get_mut(self.col)
            {
                *slot = c;
            }
            self.pending_replace = false;
            self.reset_count();
            return;
        }
        match c {
            ':' => {
                self.mode = Mode::Command;
                self.cmd = String::from(":");
                self.reset_count();
            }
            'i' => self.mode = Mode::Insert,
            'a' => {
                self.mode = Mode::Insert;
                if self.col < self.line_len() {
                    self.col += 1;
                }
            }
            'r' => self.pending_replace = true,
            'k' => {
                let n = if self.count_active { self.count.max(1) } else { 1 };
                self.row = self.row.saturating_sub(n);
                let max = self.line_len().saturating_sub(1);
                if self.col > max {
                    self.col = max;
                }
                self.reset_count();
            }
            'j' => {
                let n = if self.count_active { self.count.max(1) } else { 1 };
                self.row = (self.row + n).min(self.lines.len().saturating_sub(1));
                let max = self.line_len().saturating_sub(1);
                if self.col > max {
                    self.col = max;
                }
                self.reset_count();
            }
            'G' => {
                self.row = self.lines.len().saturating_sub(1);
                self.col = 0;
                self.reset_count();
            }
            '$' => {
                self.col = self.line_len().saturating_sub(1);
                self.reset_count();
            }
            'h' => {
                let n = if self.count_active { self.count.max(1) } else { 1 };
                self.col = self.col.saturating_sub(n);
                self.reset_count();
            }
            'l' => {
                let n = if self.count_active { self.count.max(1) } else { 1 };
                let max = self.line_len().saturating_sub(1);
                self.col = (self.col + n).min(max);
                self.reset_count();
            }
            '0' => {
                if self.count_active {
                    self.count = self.count.saturating_mul(10);
                } else {
                    self.col = 0;
                }
            }
            d @ '1'..='9' => {
                let v = (d as usize) - ('0' as usize);
                self.count = self.count.saturating_mul(10).saturating_add(v);
                self.count_active = true;
            }
            _ => self.reset_count(),
        }
    }

    fn run_command(&mut self) {
        let body = self.cmd.trim_start_matches(':').trim();
        if matches!(body, "w" | "wq" | "x" | "wa") {
            self.saves += 1;
        } else if let Ok(n) = body.parse::<usize>() {
            let last = self.lines.len().saturating_sub(1);
            self.row = n.saturating_sub(1).min(last);
            self.col = 0;
        }
    }

    fn content(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Replay an action stream through the model and return the final buffer text
/// plus the number of `:w` saves observed.
pub fn simulate(events: &[Event]) -> (String, usize) {
    let mut nv = Nvim::new();
    for ev in events {
        nv.apply(ev.action);
    }
    (nv.content(), nv.saves)
}
