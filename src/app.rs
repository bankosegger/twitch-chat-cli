use std::collections::{BTreeSet, VecDeque};

use crate::helix::{StreamInfo, StreamStatus};
use crate::irc::ChatMessage;

const MAX_MESSAGES: usize = 500;

#[derive(Debug, Clone)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

#[derive(Debug, Clone)]
pub enum StreamPanel {
    /// No TWITCH_CLIENT_ID / TWITCH_CLIENT_SECRET configured.
    Disabled,
    Loading,
    Live(StreamInfo),
    Offline,
    Error(String),
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
    pub filter: String,
    pub filter_input: Option<String>,
    pub highlight_mods: bool,
    pub highlight_subs: bool,
    pub stream_panel: StreamPanel,
    pub mods_seen: BTreeSet<String>,
}

impl App {
    pub fn new(channel: String, stream_info_enabled: bool) -> Self {
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
            filter: String::new(),
            filter_input: None,
            highlight_mods: false,
            highlight_subs: false,
            stream_panel: if stream_info_enabled {
                StreamPanel::Loading
            } else {
                StreamPanel::Disabled
            },
            mods_seen: BTreeSet::new(),
        }
    }

    pub fn set_stream_status(&mut self, status: StreamStatus) {
        self.stream_panel = match status {
            StreamStatus::Live(info) => StreamPanel::Live(info),
            StreamStatus::Offline => StreamPanel::Offline,
            StreamStatus::Error(e) => StreamPanel::Error(e),
        };
    }

    pub fn toggle_highlight_mods(&mut self) {
        self.highlight_mods = !self.highlight_mods;
    }

    pub fn toggle_highlight_subs(&mut self) {
        self.highlight_subs = !self.highlight_subs;
    }

    pub fn is_editing_filter(&self) -> bool {
        self.filter_input.is_some()
    }

    /// The filter text currently in effect for rendering: the in-progress
    /// edit buffer while editing (for live preview), else the applied filter.
    pub fn effective_filter(&self) -> &str {
        self.filter_input.as_deref().unwrap_or(&self.filter)
    }

    pub fn message_matches(&self, msg: &ChatMessage) -> bool {
        let needle = self.effective_filter();
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_lowercase();
        msg.display_name.to_lowercase().contains(&needle) || msg.text.to_lowercase().contains(&needle)
    }

    pub fn start_filter_edit(&mut self) {
        self.filter_input = Some(self.filter.clone());
    }

    pub fn filter_push_char(&mut self, c: char) {
        if let Some(buf) = &mut self.filter_input {
            buf.push(c);
        }
    }

    pub fn filter_backspace(&mut self) {
        if let Some(buf) = &mut self.filter_input {
            buf.pop();
        }
    }

    pub fn filter_clear_buffer(&mut self) {
        if let Some(buf) = &mut self.filter_input {
            buf.clear();
        }
    }

    pub fn confirm_filter(&mut self) {
        if let Some(buf) = self.filter_input.take() {
            self.filter = buf;
            self.scroll_to_bottom();
        }
    }

    pub fn cancel_filter_edit(&mut self) {
        self.filter_input = None;
    }

    pub fn push_message(&mut self, msg: ChatMessage) {
        if msg.badges.iter().any(|b| b == "MOD") {
            self.mods_seen.insert(msg.display_name.clone());
        }
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
