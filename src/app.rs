use std::collections::VecDeque;

use crate::irc::ChatMessage;

const MAX_MESSAGES: usize = 500;

#[derive(Debug, Clone)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

pub struct App {
    pub channel: String,
    pub messages: VecDeque<ChatMessage>,
    pub status: ConnStatus,
    pub last_notice: Option<String>,
    pub skip: usize,
    pub pinned_to_bottom: bool,
    pub last_total_lines: usize,
    pub last_viewport: usize,
    pub should_quit: bool,
}

impl App {
    pub fn new(channel: String) -> Self {
        Self {
            channel,
            messages: VecDeque::new(),
            status: ConnStatus::Connecting,
            last_notice: None,
            skip: 0,
            pinned_to_bottom: true,
            last_total_lines: 0,
            last_viewport: 0,
            should_quit: false,
        }
    }

    pub fn push_message(&mut self, msg: ChatMessage) {
        self.messages.push_back(msg);
        if self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }
    }

    fn max_skip(&self) -> usize {
        self.last_total_lines.saturating_sub(self.last_viewport)
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.pinned_to_bottom = false;
        self.skip = self.skip.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        let max_skip = self.max_skip();
        self.skip = (self.skip + n).min(max_skip);
        if self.skip >= max_skip {
            self.pinned_to_bottom = true;
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.pinned_to_bottom = false;
        self.skip = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.pinned_to_bottom = true;
        self.skip = self.max_skip();
    }
}
