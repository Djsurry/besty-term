use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::KeyEvent;
use ratatui::widgets::ListState;
use slack_morphism::prelude::*;
use tokio::sync::mpsc;

use crate::slack::{Conv, ConvKind, Msg, SlackContext, UserInfo};
use crate::state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    SidebarSearch,
    ChatSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Unread,
    Channels,
    External,
    Groups,
    Dms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    User,
    Channel,
}

#[derive(Debug, Clone)]
pub struct MentionEntry {
    // Visible form inserted into the input ("@David Surry", "#general", "@here").
    pub display: String,
    // Slack-API form substituted at send time ("<@U123>", "<#C123>", "<!here>").
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct MentionPopup {
    pub kind: MentionKind,
    // Byte offset of the trigger char ('@' / '#') inside `App::input`.
    pub anchor: usize,
    pub query: String,
    pub matches: Vec<MentionEntry>,
    pub selected: usize,
}

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Resize,
    UsersLoaded(HashMap<SlackUserId, UserInfo>),
    UserLoaded(UserInfo),
    ConversationsLoaded(Vec<Conv>),
    UnreadCountLoaded(SlackChannelId, u32, Option<SlackTs>),
    MutedChannelsLoaded(HashSet<SlackChannelId>),
    ImPartner(SlackChannelId, Option<SlackUserId>),
    ConvMembers(SlackChannelId, Vec<SlackUserId>),
    HistoryLoaded(SlackChannelId, Vec<Msg>),
    HistoryFailed(SlackChannelId, String),
    NewMessages(SlackChannelId, Vec<Msg>),
    ThreadReplies(SlackChannelId, SlackTs, Vec<Msg>),
    SendOk,
    Error(String),
}

pub struct App {
    pub slack: Arc<SlackContext>,
    pub tx: mpsc::UnboundedSender<AppEvent>,

    pub users: HashMap<SlackUserId, UserInfo>,
    pub convs: Vec<Conv>,
    pub messages: HashMap<SlackChannelId, Vec<Msg>>,
    // Channels for which conversations.history has actually returned. Distinct
    // from `messages.contains_key`, which is true even when only a single
    // socket-delivered message has been stashed without backfill.
    pub history_loaded: HashSet<SlackChannelId>,
    pub loading: HashSet<SlackChannelId>,
    pub unread: HashMap<SlackChannelId, u32>,
    pub user_lookups: HashSet<SlackUserId>,
    pub im_partner_requested: HashSet<SlackChannelId>,
    pub poll_in_flight: HashSet<SlackChannelId>,
    pub muted: HashSet<SlackChannelId>,

    pub sidebar_state: ListState,
    pub mode: Mode,
    pub input: String,
    pub status: String,
    pub error: Option<(String, Instant)>,
    pub message_scroll: u16,

    pub sidebar_query: String,
    pub chat_query: String,
    pub chat_matches: Vec<usize>,
    pub chat_match_idx: usize,
    pub pending_g: bool,
    pub pending_z: bool,
    pub pending_d: bool,
    pub hidden: HashSet<SlackChannelId>,
    pub sidebar_view_h: u16,

    pub last_history_fetch: HashMap<SlackChannelId, Instant>,
    pub previously_selected: Option<SlackChannelId>,
    pub conv_members: HashMap<SlackChannelId, Vec<SlackUserId>>,
    pub last_marked_ts: HashMap<SlackChannelId, SlackTs>,

    pub mention_popup: Option<MentionPopup>,
    // (display_form, slack_token) substitutions applied at send time. Lets the
    // input box show readable text like "@David Surry" while ensuring the
    // posted message contains the real `<@U123>` mention.
    pub pending_mentions: Vec<(String, String)>,

    // Set whenever cached state changes; the tick handler flushes to disk so
    // bursty events (e.g. ImPartner for every DM at startup) coalesce into a
    // single write instead of one rename per event.
    pub state_dirty: bool,
}

// We rely on socket mode for real-time message delivery; this poll is a
// safety net that reconciles any events socket mode might drop. Once a
// minute is plenty for that role and keeps idle CPU near zero.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const ERROR_TTL: Duration = Duration::from_secs(6);
// Hide DMs/groups whose latest activity is older than this. Mirrors the
// "out of sight" behavior Slack already applies via is_dormant, but tighter
// so the sidebar stays focused on live conversations.
const DM_INACTIVE_CUTOFF: Duration = Duration::from_secs(14 * 24 * 60 * 60);

fn ts_seconds(ts: &SlackTs) -> Option<f64> {
    ts.0.parse::<f64>().ok()
}

impl App {
    pub fn new(slack: Arc<SlackContext>, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        let mut sidebar_state = ListState::default();
        sidebar_state.select(Some(0));
        let team = slack.team.clone();
        // Warm-start from the on-disk cache so the sidebar paints immediately;
        // the background loaders will refresh users/convs and replace these.
        let cached = state::load();
        let (hidden, users, convs) = match cached.as_ref() {
            Some(s) => (
                state::rebuild_hidden(s),
                state::rebuild_users(s),
                state::rebuild_convs(s),
            ),
            None => (HashSet::new(), HashMap::new(), Vec::new()),
        };
        let status = if cached.is_some() {
            format!("connected: {} · cached", team)
        } else {
            format!("connected: {}", team)
        };
        Self {
            slack,
            tx,
            users,
            convs,
            messages: HashMap::new(),
            history_loaded: HashSet::new(),
            loading: HashSet::new(),
            unread: HashMap::new(),
            user_lookups: HashSet::new(),
            im_partner_requested: HashSet::new(),
            poll_in_flight: HashSet::new(),
            muted: HashSet::new(),
            sidebar_state,
            mode: Mode::Normal,
            input: String::new(),
            status,
            error: None,
            message_scroll: 0,
            sidebar_query: String::new(),
            chat_query: String::new(),
            chat_matches: Vec::new(),
            chat_match_idx: 0,
            pending_g: false,
            pending_z: false,
            pending_d: false,
            hidden,
            sidebar_view_h: 0,
            last_history_fetch: HashMap::new(),
            previously_selected: None,
            conv_members: HashMap::new(),
            last_marked_ts: HashMap::new(),
            mention_popup: None,
            pending_mentions: Vec::new(),
            state_dirty: false,
        }
    }

    fn mark_state_dirty(&mut self) {
        self.state_dirty = true;
    }

    pub fn flush_state_if_dirty(&mut self) {
        if !self.state_dirty {
            return;
        }
        self.state_dirty = false;
        let snapshot = state::snapshot(&self.hidden, &self.users, &self.convs);
        tokio::spawn(async move {
            if let Err(e) = state::save(&snapshot) {
                debug_log(&format!("state save failed: {e:#}"));
            }
        });
    }

    pub fn hide_selected(&mut self) {
        let Some(c) = self.selected_conv().cloned() else {
            return;
        };
        let prev_row = self.sidebar_state.selected();
        self.hidden.insert(c.id.clone());
        self.unread.remove(&c.id);
        self.previously_selected = None;
        self.mark_state_dirty();
        // After hide, the row index that used to point at this conv now points
        // at whatever was directly below it. Keep the cursor there (skipping
        // headers) instead of jumping back to the top.
        let rows = self.flat_visible();
        if rows.is_empty() {
            self.sidebar_state.select(None);
            return;
        }
        let start = prev_row.unwrap_or(0).min(rows.len() - 1);
        let pick = (start..rows.len())
            .chain((0..start).rev())
            .find(|i| matches!(rows[*i], VisibleRow::Conv(_)));
        if let Some(i) = pick {
            self.sidebar_state.select(Some(i));
            self.on_selection_changed();
        } else {
            self.sidebar_state.select(None);
        }
    }

    fn mark_read_latest(&mut self, ch: &SlackChannelId) {
        let Some(msgs) = self.messages.get(ch) else {
            return;
        };
        let Some(latest) = msgs.last().map(|m| m.ts.clone()) else {
            return;
        };
        if self.last_marked_ts.get(ch) == Some(&latest) {
            return;
        }
        self.last_marked_ts.insert(ch.clone(), latest.clone());
        let slack = self.slack.clone();
        let ch_owned = ch.clone();
        tokio::spawn(async move {
            if let Err(e) = slack.mark_read(&ch_owned, &latest).await {
                debug_log(&format!("mark_read {} failed: {e:#}", ch_owned.0));
            }
        });
    }

    fn matches_sidebar_query(&self, c: &Conv) -> bool {
        if self.sidebar_query.is_empty() {
            return true;
        }
        let q = self.sidebar_query.to_lowercase();
        self.display_name(c).to_lowercase().contains(&q)
            || c.name.to_lowercase().contains(&q)
    }

    pub fn sorted_convs(
        &self,
    ) -> (Vec<usize>, Vec<usize>, Vec<usize>, Vec<usize>, Vec<usize>) {
        let mut unread = Vec::new();
        let mut channels = Vec::new();
        let mut external = Vec::new();
        let mut groups = Vec::new();
        let mut dms = Vec::new();
        let inactive_cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| d.as_secs().checked_sub(DM_INACTIVE_CUTOFF.as_secs()))
            .map(|s| s as f64);
        for (idx, c) in self.convs.iter().enumerate() {
            if c.is_disabled {
                continue;
            }
            // Slack's own sidebar hides DMs/MPIMs the user has closed (is_open=false);
            // mirror that so besty-term doesn't show stale conversations.
            if !c.is_open && matches!(c.kind, ConvKind::Im { .. } | ConvKind::Mpim) {
                continue;
            }
            if let ConvKind::Im { user_id: Some(uid) } = &c.kind {
                if self.users.get(uid).map(|u| u.deleted).unwrap_or(false) {
                    continue;
                }
            }
            if matches!(c.kind, ConvKind::Mpim) && self.mpim_has_inactive_member(&c.id) {
                continue;
            }
            if !self.matches_sidebar_query(c) {
                continue;
            }
            let is_muted = c.is_muted || self.muted.contains(&c.id);
            let has_unread =
                !is_muted && self.unread.get(&c.id).copied().unwrap_or(0) > 0;
            if self.hidden.contains(&c.id) && !has_unread {
                continue;
            }
            // Hide DMs/MPIMs with no activity in the last 14 days. Unread or
            // explicit-search bypass keeps users from losing visibility on
            // something they care about. When we don't yet know the latest ts
            // (the per-channel probe hasn't landed), keep the conv visible —
            // filtering on absence would briefly hide everything at startup.
            if matches!(c.kind, ConvKind::Im { .. } | ConvKind::Mpim)
                && !has_unread
                && self.sidebar_query.is_empty()
            {
                if let (Some(ts), Some(cutoff)) = (
                    c.last_activity_ts.as_ref().and_then(ts_seconds),
                    inactive_cutoff,
                ) {
                    if ts < cutoff {
                        continue;
                    }
                }
            }
            // Slack flags inactive conversations as `properties.is_dormant` and
            // hides them from its own sidebar. Mirror that — unless the channel
            // is currently unread or matching a search query, in which case the
            // user explicitly wants to see it.
            if c.is_dormant && !has_unread && self.sidebar_query.is_empty() {
                continue;
            }
            if has_unread {
                if let ConvKind::Channel { .. } = &c.kind {
                    if !c.is_member {
                        continue;
                    }
                }
                unread.push(idx);
                continue;
            }
            match &c.kind {
                ConvKind::Channel { .. } => {
                    if c.is_member {
                        if c.is_ext_shared {
                            external.push(idx);
                        } else {
                            channels.push(idx);
                        }
                    }
                }
                ConvKind::Mpim => groups.push(idx),
                ConvKind::Im { .. } => dms.push(idx),
            }
        }
        let by_name = |a: &usize, b: &usize| self.convs[*a].name.cmp(&self.convs[*b].name);
        let by_display = |a: &usize, b: &usize| {
            self.display_name(&self.convs[*a])
                .to_lowercase()
                .cmp(&self.display_name(&self.convs[*b]).to_lowercase())
        };
        let unread_count = |i: usize| self.unread.get(&self.convs[i].id).copied().unwrap_or(0);
        // Unread: by count desc, then alphabetical (display name handles DMs/mpims).
        unread.sort_by(|a, b| {
            unread_count(*b)
                .cmp(&unread_count(*a))
                .then_with(|| by_display(a, b))
        });
        // Within remaining sections: muted last, then alphabetical.
        let muted_rank = |i: usize| -> u8 { if self.convs[i].is_muted { 1 } else { 0 } };
        channels.sort_by(|a, b| muted_rank(*a).cmp(&muted_rank(*b)).then_with(|| by_name(a, b)));
        external.sort_by(|a, b| muted_rank(*a).cmp(&muted_rank(*b)).then_with(|| by_name(a, b)));
        // DMs/groups: muted last, then most-recent activity first (alpha as tiebreak).
        let activity_seconds = |i: usize| -> f64 {
            self.convs[i]
                .last_activity_ts
                .as_ref()
                .and_then(ts_seconds)
                .unwrap_or(f64::NEG_INFINITY)
        };
        let by_recency = |a: &usize, b: &usize| {
            activity_seconds(*b)
                .partial_cmp(&activity_seconds(*a))
                .unwrap_or(std::cmp::Ordering::Equal)
        };
        groups.sort_by(|a, b| {
            muted_rank(*a)
                .cmp(&muted_rank(*b))
                .then_with(|| by_recency(a, b))
                .then_with(|| by_name(a, b))
        });
        dms.sort_by(|a, b| {
            muted_rank(*a)
                .cmp(&muted_rank(*b))
                .then_with(|| by_recency(a, b))
                .then_with(|| by_display(a, b))
        });
        (unread, channels, external, groups, dms)
    }

    pub fn flat_visible(&self) -> Vec<VisibleRow> {
        let (unread, channels, external, groups, dms) = self.sorted_convs();
        let mut rows = Vec::new();
        let push_section = |rows: &mut Vec<VisibleRow>, section: Section, idxs: &[usize], force: bool| {
            if !idxs.is_empty() || force {
                rows.push(VisibleRow::Header(section, idxs.len()));
                for i in idxs {
                    rows.push(VisibleRow::Conv(*i));
                }
            }
        };
        push_section(&mut rows, Section::Unread, &unread, false);
        push_section(&mut rows, Section::Channels, &channels, true);
        push_section(&mut rows, Section::External, &external, false);
        push_section(&mut rows, Section::Dms, &dms, true);
        push_section(&mut rows, Section::Groups, &groups, false);
        rows
    }

    pub fn selected_conv_idx(&self) -> Option<usize> {
        let rows = self.flat_visible();
        let sel = self.sidebar_state.selected()?;
        rows.get(sel).and_then(|r| match r {
            VisibleRow::Conv(i) => Some(*i),
            _ => None,
        })
    }

    pub fn selected_conv(&self) -> Option<&Conv> {
        self.selected_conv_idx().and_then(|i| self.convs.get(i))
    }

    pub fn move_selection(&mut self, delta: i32) {
        let rows = self.flat_visible();
        let conv_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, VisibleRow::Conv(_)).then_some(i))
            .collect();
        if conv_positions.is_empty() {
            self.sidebar_state.select(None);
            return;
        }
        let cur = self.sidebar_state.selected().unwrap_or(conv_positions[0]);
        let cur_idx = conv_positions
            .iter()
            .position(|&p| p == cur)
            .or_else(|| {
                if delta >= 0 {
                    conv_positions.iter().position(|&p| p >= cur)
                } else {
                    conv_positions.iter().rposition(|&p| p <= cur)
                }
            })
            .unwrap_or(0);
        let target = (cur_idx as i32 + delta)
            .clamp(0, conv_positions.len() as i32 - 1) as usize;
        self.sidebar_state.select(Some(conv_positions[target]));
        self.message_scroll = 0;
        self.on_selection_changed();
    }

    pub fn jump_first_conv(&mut self) {
        let rows = self.flat_visible();
        for (i, r) in rows.iter().enumerate() {
            if matches!(r, VisibleRow::Conv(_)) {
                self.sidebar_state.select(Some(i));
                self.on_selection_changed();
                return;
            }
        }
    }

    pub fn center_selection(&mut self) {
        let Some(sel) = self.sidebar_state.selected() else {
            return;
        };
        let view_h = self.sidebar_view_h as usize;
        if view_h == 0 {
            return;
        }
        let half = view_h / 2;
        let target_offset = sel.saturating_sub(half);
        *self.sidebar_state.offset_mut() = target_offset;
    }

    pub fn jump_section(&mut self, delta: i32) {
        let rows = self.flat_visible();
        if rows.is_empty() {
            return;
        }
        let headers: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, VisibleRow::Header(_, _)).then_some(i))
            .collect();
        if headers.is_empty() {
            return;
        }
        let sel = self.sidebar_state.selected().unwrap_or(0);
        let cur_section = headers
            .iter()
            .rposition(|&h| h <= sel)
            .unwrap_or(0);
        let target = if delta > 0 {
            (cur_section + 1).min(headers.len() - 1)
        } else if cur_section > 0 && sel == headers[cur_section] + 1 {
            cur_section - 1
        } else {
            cur_section
        };
        let header_pos = headers[target];
        let first_conv = rows
            .iter()
            .enumerate()
            .skip(header_pos + 1)
            .find_map(|(i, r)| matches!(r, VisibleRow::Conv(_)).then_some(i));
        if let Some(pos) = first_conv {
            self.sidebar_state.select(Some(pos));
            self.on_selection_changed();
        }
    }

    pub fn jump_last_conv(&mut self) {
        let rows = self.flat_visible();
        for (i, r) in rows.iter().enumerate().rev() {
            if matches!(r, VisibleRow::Conv(_)) {
                self.sidebar_state.select(Some(i));
                self.on_selection_changed();
                return;
            }
        }
    }

    fn on_selection_changed(&mut self) {
        self.chat_matches.clear();
        self.chat_match_idx = 0;
        let new_id = self.selected_conv().map(|c| c.id.clone());

        // Clear the unread badge for the conv we just left (not the one we
        // just landed on) — that way the conv under the cursor stays put in
        // the UNREAD section instead of jumping into its home section the
        // moment it's highlighted.
        let mut moved_out: Option<SlackChannelId> = None;
        if let Some(prev) = self.previously_selected.clone() {
            if Some(&prev) != new_id.as_ref() {
                if self.unread.remove(&prev).is_some() {
                    moved_out = Some(prev);
                }
            }
        }

        if let Some(c) = self.selected_conv().cloned() {
            self.message_scroll = 0;
            self.request_history(&c.id);
            self.request_im_partner_if_needed(&c);
            if !self.chat_query.is_empty() {
                self.recompute_chat_matches();
            }
        }

        // Removing the prev's unread may shift rows; refocus the cursor on
        // the conv the user actually navigated to.
        if moved_out.is_some() {
            if let Some(id) = &new_id {
                self.refocus_selected(id);
            }
        }
        self.previously_selected = new_id;
    }

    fn refocus_selected(&mut self, id: &SlackChannelId) {
        let rows = self.flat_visible();
        for (i, r) in rows.iter().enumerate() {
            if let VisibleRow::Conv(idx) = r {
                if &self.convs[*idx].id == id {
                    self.sidebar_state.select(Some(i));
                    return;
                }
            }
        }
    }

    fn mpim_has_inactive_member(&self, ch: &SlackChannelId) -> bool {
        let Some(members) = self.conv_members.get(ch) else {
            // Members not fetched yet — give it the benefit of the doubt.
            return false;
        };
        members
            .iter()
            .filter(|u| *u != &self.slack.me)
            .any(|u| {
                self.users
                    .get(u)
                    .map(|info| info.deleted)
                    .unwrap_or(false)
            })
    }

    fn ensure_self_dm(&mut self) {
        let me = self.slack.me.clone();
        let has_self = self.convs.iter().any(|c| match &c.kind {
            ConvKind::Im { user_id: Some(u) } => u == &me,
            _ => false,
        });
        if has_self {
            return;
        }
        let tx = self.tx.clone();
        let slack = self.slack.clone();
        tokio::spawn(async move {
            match slack.open_self_dm().await {
                Ok(ch) => {
                    let _ = tx.send(AppEvent::ImPartner(ch, Some(slack.me.clone())));
                }
                Err(_) => {}
            }
        });
    }

    fn request_im_partner_if_needed(&mut self, c: &Conv) {
        if let ConvKind::Im { user_id: None } = c.kind {
            if !self.im_partner_requested.contains(&c.id) {
                self.im_partner_requested.insert(c.id.clone());
                let tx = self.tx.clone();
                let slack = self.slack.clone();
                let ch = c.id.clone();
                tokio::spawn(async move {
                    let u = slack.im_partner(&ch).await.ok().flatten();
                    let _ = tx.send(AppEvent::ImPartner(ch, u));
                });
            }
        }
    }

    pub fn request_history(&mut self, channel: &SlackChannelId) {
        if self.loading.contains(channel) || self.history_loaded.contains(channel) {
            return;
        }
        if let Some(c) = self.convs.iter().find(|c| &c.id == channel) {
            if c.is_disabled {
                return;
            }
        }
        self.loading.insert(channel.clone());
        debug_log(&format!("history request {}", channel.0));
        let tx = self.tx.clone();
        let slack = self.slack.clone();
        let ch = channel.clone();
        tokio::spawn(async move {
            match slack.history(&ch, 80).await {
                Ok(msgs) => {
                    debug_log(&format!("history ok {} -> {} msgs", ch.0, msgs.len()));
                    let _ = tx.send(AppEvent::HistoryLoaded(ch, msgs));
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::HistoryFailed(ch, format!("{e:#}")));
                }
            }
        });
    }

    pub fn poll_selected(&mut self) {
        let Some(c) = self.selected_conv().cloned() else {
            return;
        };
        if c.is_disabled || self.poll_in_flight.contains(&c.id) {
            return;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_history_fetch.get(&c.id) {
            if now.duration_since(*prev) < POLL_INTERVAL {
                return;
            }
        }
        let Some(latest) = self
            .messages
            .get(&c.id)
            .and_then(|m| m.last().map(|m| m.ts.clone()))
        else {
            return;
        };
        self.last_history_fetch.insert(c.id.clone(), now);
        self.poll_in_flight.insert(c.id.clone());
        let tx = self.tx.clone();
        let slack = self.slack.clone();
        let ch = c.id.clone();
        tokio::spawn(async move {
            match slack.history_after(&ch, &latest, 50).await {
                Ok(msgs) => {
                    let _ = tx.send(AppEvent::NewMessages(ch, msgs));
                }
                Err(_) => {
                    let _ = tx.send(AppEvent::NewMessages(ch, Vec::new()));
                }
            }
        });
    }

    pub fn on_event(&mut self, ev: AppEvent) -> ControlFlow {
        match ev {
            AppEvent::Tick => {
                if let Some((_, at)) = &self.error {
                    if at.elapsed() > ERROR_TTL {
                        self.error = None;
                    }
                }
                self.poll_selected();
                self.flush_state_if_dirty();
            }
            AppEvent::Resize => {}
            AppEvent::Key(_) => unreachable!("keys handled by caller"),
            AppEvent::UsersLoaded(users) => {
                self.users = users;
                self.status =
                    format!("connected: {} · {} users", self.slack.team, self.users.len());
                self.mark_state_dirty();
            }
            AppEvent::UserLoaded(u) => {
                self.user_lookups.remove(&u.id);
                self.users.insert(u.id.clone(), u);
                self.mark_state_dirty();
            }
            AppEvent::ConversationsLoaded(mut convs) => {
                // Preserve resolutions we built up in this session (or loaded
                // from cache) that conversations.list doesn't itself supply:
                //   - IM partner user_id  (lookup via conversations.members)
                //   - last_activity_ts    (lookup via conversations.info)
                // Otherwise the fresh list snaps every DM back to a raw ID
                // and bumps last-activity sort order until probes re-run.
                let prior_im_user: HashMap<SlackChannelId, SlackUserId> = self
                    .convs
                    .iter()
                    .filter_map(|c| match &c.kind {
                        ConvKind::Im { user_id: Some(uid) } => Some((c.id.clone(), uid.clone())),
                        _ => None,
                    })
                    .collect();
                let prior_activity: HashMap<SlackChannelId, SlackTs> = self
                    .convs
                    .iter()
                    .filter_map(|c| c.last_activity_ts.clone().map(|t| (c.id.clone(), t)))
                    .collect();
                for c in convs.iter_mut() {
                    c.is_muted = self.muted.contains(&c.id);
                    if let ConvKind::Im { user_id: None } = &c.kind {
                        if let Some(uid) = prior_im_user.get(&c.id).cloned() {
                            c.kind = ConvKind::Im { user_id: Some(uid) };
                        }
                    }
                    if c.last_activity_ts.is_none() {
                        if let Some(ts) = prior_activity.get(&c.id).cloned() {
                            c.last_activity_ts = Some(ts);
                        }
                    }
                }
                self.convs = convs;
                self.ensure_self_dm();
                self.jump_first_conv();
                self.mark_state_dirty();
                let channels: Vec<SlackChannelId> = self
                    .convs
                    .iter()
                    .filter(|c| !c.is_disabled && c.is_member)
                    .map(|c| c.id.clone())
                    .collect();
                spawn_unread_fetcher(self.slack.clone(), self.tx.clone(), channels);
                let ims: Vec<SlackChannelId> = self
                    .convs
                    .iter()
                    .filter(|c| matches!(c.kind, ConvKind::Im { user_id: None }))
                    .map(|c| c.id.clone())
                    .collect();
                for ch in &ims {
                    self.im_partner_requested.insert(ch.clone());
                }
                let mpims: Vec<SlackChannelId> = self
                    .convs
                    .iter()
                    .filter(|c| matches!(c.kind, ConvKind::Mpim))
                    .map(|c| c.id.clone())
                    .collect();
                let tx = self.tx.clone();
                let slack = self.slack.clone();
                tokio::spawn(async move {
                    for ch in ims {
                        let members = slack.conversation_members(&ch).await.unwrap_or_default();
                        let partner = members.iter().find(|u| *u != &slack.me).cloned();
                        let _ = tx.send(AppEvent::ConvMembers(ch.clone(), members));
                        let _ = tx.send(AppEvent::ImPartner(ch, partner));
                    }
                    for ch in mpims {
                        let members = slack.conversation_members(&ch).await.unwrap_or_default();
                        let _ = tx.send(AppEvent::ConvMembers(ch, members));
                    }
                });
            }
            AppEvent::UnreadCountLoaded(ch, count, latest_ts) => {
                let is_selected = self.selected_conv().map(|c| c.id.clone()) == Some(ch.clone());
                debug_log(&format!(
                    "unread {} count={} selected={}",
                    ch.0, count, is_selected
                ));
                if let Some(ts) = latest_ts {
                    if let Some(conv) = self.convs.iter_mut().find(|c| c.id == ch) {
                        let bump = conv
                            .last_activity_ts
                            .as_ref()
                            .and_then(ts_seconds)
                            .map(|cur| ts_seconds(&ts).unwrap_or(0.0) > cur)
                            .unwrap_or(true);
                        if bump {
                            conv.last_activity_ts = Some(ts);
                        }
                    }
                }
                if is_selected || count == 0 {
                    self.unread.remove(&ch);
                } else {
                    if self.hidden.remove(&ch) {
                        debug_log(&format!("hidden conv {} woken by unread", ch.0));
                        self.mark_state_dirty();
                    }
                    self.unread.insert(ch, count);
                }
            }
            AppEvent::MutedChannelsLoaded(set) => {
                for c in self.convs.iter_mut() {
                    c.is_muted = set.contains(&c.id);
                }
                self.muted = set;
            }
            AppEvent::ConvMembers(ch, members) => {
                self.resolve_unknown_user_ids(&members);
                self.conv_members.insert(ch, members);
            }
            AppEvent::ImPartner(ch, user_id) => {
                let resolved = user_id.or_else(|| Some(self.slack.me.clone()));
                self.mark_state_dirty();
                if let Some(conv) = self.convs.iter_mut().find(|c| c.id == ch) {
                    conv.kind = ConvKind::Im { user_id: resolved };
                } else {
                    let is_muted = self.muted.contains(&ch);
                    self.convs.push(Conv {
                        id: ch.clone(),
                        name: ch.0.clone(),
                        kind: ConvKind::Im { user_id: resolved },
                        is_member: true,
                        is_disabled: false,
                        is_muted,
                        is_open: true,
                        is_ext_shared: false,
                        is_dormant: false,
                        last_activity_ts: None,
                    });
                }
            }
            AppEvent::HistoryLoaded(ch, msgs) => {
                self.loading.remove(&ch);
                self.history_loaded.insert(ch.clone());
                self.resolve_unknown_authors(&msgs);
                // Capture parents-with-replies before moving msgs into storage
                // so we can backfill thread context (conversations.history
                // returns top-level messages only — replies live in their
                // own endpoint).
                let parents_with_replies: Vec<SlackTs> = msgs
                    .iter()
                    .filter(|m| m.reply_count > 0)
                    .map(|m| m.ts.clone())
                    .collect();
                // Merge with anything socket mode already delivered for this
                // channel (e.g. an unread DM where a new message arrived
                // before the user opened the conversation). Dedupe by ts.
                let prior = self.messages.remove(&ch).unwrap_or_default();
                let mut merged = msgs;
                let known: HashSet<_> = merged.iter().map(|m| m.ts.clone()).collect();
                for m in prior {
                    if !known.contains(&m.ts) {
                        merged.push(m);
                    }
                }
                merged.sort_by(|a, b| a.ts.0.cmp(&b.ts.0));
                self.messages.insert(ch.clone(), merged);
                let is_selected = self.selected_conv().map(|c| c.id.clone()) == Some(ch.clone());
                if is_selected {
                    if !self.chat_query.is_empty() {
                        self.recompute_chat_matches();
                    }
                    self.mark_read_latest(&ch);
                }
                spawn_thread_replies_fetcher(
                    self.slack.clone(),
                    self.tx.clone(),
                    ch,
                    parents_with_replies,
                );
            }
            AppEvent::ThreadReplies(ch, parent_ts, replies) => {
                if replies.is_empty() {
                    return ControlFlow::Continue;
                }
                self.resolve_unknown_authors(&replies);
                let entry = self.messages.entry(ch.clone()).or_default();
                let known: HashSet<_> = entry.iter().map(|m| m.ts.clone()).collect();
                let added: Vec<Msg> =
                    replies.into_iter().filter(|m| !known.contains(&m.ts)).collect();
                if added.is_empty() {
                    return ControlFlow::Continue;
                }
                entry.extend(added);
                entry.sort_by(|a, b| a.ts.0.cmp(&b.ts.0));
                let _ = parent_ts; // reserved for future "scroll to parent" UX
                let is_selected = self.selected_conv().map(|c| c.id.clone()) == Some(ch);
                if is_selected && !self.chat_query.is_empty() {
                    self.recompute_chat_matches();
                }
            }
            AppEvent::HistoryFailed(ch, reason) => {
                self.loading.remove(&ch);
                debug_log(&format!("history failed {}: {}", ch.0, reason));
                let dead = reason.contains("channel_not_found")
                    || reason.contains("is_archived")
                    || reason.contains("user_not_found")
                    || reason.contains("user_is_inactive");
                if dead {
                    if let Some(conv) = self.convs.iter_mut().find(|c| c.id == ch) {
                        conv.is_disabled = true;
                    }
                    let was_selected = self.selected_conv().map(|c| c.id.clone()) == Some(ch);
                    if was_selected {
                        self.jump_first_conv();
                    }
                } else {
                    self.error = Some((format!("history: {reason}"), Instant::now()));
                }
            }
            AppEvent::NewMessages(ch, msgs) => {
                self.poll_in_flight.remove(&ch);
                if msgs.is_empty() {
                    return ControlFlow::Continue;
                }
                self.resolve_unknown_authors(&msgs);
                let newest_ts = msgs.iter().map(|m| m.ts.clone()).max_by(|a, b| {
                    a.0.parse::<f64>()
                        .unwrap_or(0.0)
                        .partial_cmp(&b.0.parse::<f64>().unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                if let Some(ts) = newest_ts {
                    if let Some(conv) = self.convs.iter_mut().find(|c| c.id == ch) {
                        let bump = conv
                            .last_activity_ts
                            .as_ref()
                            .and_then(ts_seconds)
                            .map(|cur| ts_seconds(&ts).unwrap_or(0.0) > cur)
                            .unwrap_or(true);
                        if bump {
                            conv.last_activity_ts = Some(ts);
                        }
                    }
                }
                // If any new message is a thread reply and we don't yet have
                // the parent + sibling replies, queue a fetch so the user
                // sees the conversation in context rather than a lone "↳ …".
                let entry = self.messages.entry(ch.clone()).or_default();
                let known: HashSet<_> = entry.iter().map(|m| m.ts.clone()).collect();
                let mut thread_parents_to_fetch: Vec<SlackTs> = Vec::new();
                let mut seen_parents: HashSet<SlackTs> = HashSet::new();
                for m in &msgs {
                    if let Some(parent_ts) = m.thread_ts.clone() {
                        if seen_parents.insert(parent_ts.clone())
                            && !known.contains(&parent_ts)
                        {
                            thread_parents_to_fetch.push(parent_ts);
                        }
                    }
                }
                let mut new = 0;
                for m in msgs {
                    if !known.contains(&m.ts) {
                        entry.push(m);
                        new += 1;
                    }
                }
                if !thread_parents_to_fetch.is_empty() {
                    // conversations.replies returns parent + replies in one
                    // call, so a single fetch backfills the whole thread for
                    // any reply we receive without the parent in view.
                    spawn_thread_replies_fetcher(
                        self.slack.clone(),
                        self.tx.clone(),
                        ch.clone(),
                        thread_parents_to_fetch,
                    );
                }
                let is_selected = self.selected_conv().map(|c| c.id.clone()) == Some(ch.clone());
                if new > 0 && !is_selected {
                    if self.hidden.remove(&ch) {
                        debug_log(&format!("hidden conv {} woken by new msg", ch.0));
                        self.mark_state_dirty();
                    }
                    *self.unread.entry(ch.clone()).or_default() += new;
                }
                if is_selected {
                    self.mark_read_latest(&ch);
                }
            }
            AppEvent::SendOk => {}
            AppEvent::Error(msg) => {
                self.error = Some((msg, Instant::now()));
            }
        }
        ControlFlow::Continue
    }

    pub fn submit_input(&mut self) -> Result<()> {
        let mut text = self.input.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let Some(c) = self.selected_conv().cloned() else {
            return Ok(());
        };
        // Replace each "@Display Name" / "#channel" the user accepted from the
        // popup with the real Slack token so the post actually mentions them.
        // First-match-wins handles repeated picks of the same person in order.
        for (display, token) in std::mem::take(&mut self.pending_mentions) {
            if let Some(idx) = text.find(&display) {
                text.replace_range(idx..idx + display.len(), &token);
            }
        }
        self.input.clear();
        self.mention_popup = None;
        let tx = self.tx.clone();
        let slack = self.slack.clone();
        let ch = c.id.clone();
        tokio::spawn(async move {
            match slack.post_message(&ch, &text).await {
                Ok(_) => {
                    let _ = tx.send(AppEvent::SendOk);
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Error(format!("send: {e:#}")));
                }
            }
        });
        Ok(())
    }

    pub fn maybe_open_mention(&mut self, trigger: char) {
        let kind = match trigger {
            '@' => MentionKind::User,
            '#' => MentionKind::Channel,
            _ => return,
        };
        // Trigger has already been pushed onto `self.input`. Only open at the
        // start of the input or after whitespace so we don't fire inside
        // emails ("user@host") or hashtags pasted from elsewhere.
        let trigger_len = trigger.len_utf8();
        if self.input.len() < trigger_len {
            return;
        }
        let anchor = self.input.len() - trigger_len;
        let prev = self.input[..anchor].chars().next_back();
        match prev {
            None => {}
            Some(c) if c.is_whitespace() => {}
            _ => return,
        }
        let matches = self.compute_mention_matches(kind, "");
        self.mention_popup = Some(MentionPopup {
            kind,
            anchor,
            query: String::new(),
            matches,
            selected: 0,
        });
    }

    pub fn update_mention_query(&mut self) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        let anchor = popup.anchor;
        let trigger_len = 1; // both '@' and '#' are single-byte ASCII
        if anchor + trigger_len > self.input.len() {
            // Anchor char was deleted (e.g. backspace).
            self.mention_popup = None;
            return;
        }
        let tail = &self.input[anchor + trigger_len..];
        if tail.contains(char::is_whitespace) {
            // User moved past the mention boundary by typing a space.
            self.mention_popup = None;
            return;
        }
        let kind = popup.kind;
        let query = tail.to_string();
        let matches = self.compute_mention_matches(kind, &query);
        if let Some(popup) = self.mention_popup.as_mut() {
            popup.query = query;
            popup.matches = matches;
            if popup.selected >= popup.matches.len() {
                popup.selected = popup.matches.len().saturating_sub(1);
            }
        }
    }

    pub fn close_mention_popup(&mut self) {
        self.mention_popup = None;
    }

    pub fn mention_move(&mut self, delta: isize) {
        let Some(popup) = self.mention_popup.as_mut() else {
            return;
        };
        if popup.matches.is_empty() {
            return;
        }
        let len = popup.matches.len() as isize;
        let next = (popup.selected as isize + delta).rem_euclid(len);
        popup.selected = next as usize;
    }

    pub fn accept_mention(&mut self) -> bool {
        let Some(popup) = self.mention_popup.take() else {
            return false;
        };
        let Some(entry) = popup.matches.get(popup.selected).cloned() else {
            return false;
        };
        self.input.truncate(popup.anchor);
        self.input.push_str(&entry.display);
        self.input.push(' ');
        self.pending_mentions.push((entry.display, entry.token));
        true
    }

    fn compute_mention_matches(&self, kind: MentionKind, query: &str) -> Vec<MentionEntry> {
        let q = query.to_lowercase();
        match kind {
            MentionKind::User => {
                let mut out: Vec<MentionEntry> = Vec::new();
                for name in ["here", "channel", "everyone"] {
                    if q.is_empty() || name.starts_with(&q) {
                        out.push(MentionEntry {
                            display: format!("@{name}"),
                            token: format!("<!{name}>"),
                        });
                    }
                }
                let mut users: Vec<&UserInfo> = self
                    .users
                    .values()
                    .filter(|u| !u.deleted)
                    .filter(|u| {
                        q.is_empty() || u.display.to_lowercase().contains(&q)
                    })
                    .collect();
                users.sort_by(|a, b| {
                    let al = a.display.to_lowercase();
                    let bl = b.display.to_lowercase();
                    let asw = !q.is_empty() && al.starts_with(&q);
                    let bsw = !q.is_empty() && bl.starts_with(&q);
                    bsw.cmp(&asw).then_with(|| al.cmp(&bl))
                });
                for u in users.into_iter().take(20) {
                    out.push(MentionEntry {
                        display: format!("@{}", u.display),
                        token: format!("<@{}>", u.id.0),
                    });
                }
                out
            }
            MentionKind::Channel => {
                let mut convs: Vec<&Conv> = self
                    .convs
                    .iter()
                    .filter(|c| matches!(c.kind, ConvKind::Channel { .. }) && !c.is_disabled)
                    .filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q))
                    .collect();
                convs.sort_by(|a, b| {
                    let al = a.name.to_lowercase();
                    let bl = b.name.to_lowercase();
                    let asw = !q.is_empty() && al.starts_with(&q);
                    let bsw = !q.is_empty() && bl.starts_with(&q);
                    bsw.cmp(&asw).then_with(|| al.cmp(&bl))
                });
                convs
                    .into_iter()
                    .take(20)
                    .map(|c| MentionEntry {
                        display: format!("#{}", c.name),
                        token: format!("<#{}>", c.id.0),
                    })
                    .collect()
            }
        }
    }

    fn resolve_unknown_authors(&mut self, msgs: &[Msg]) {
        for m in msgs {
            if let Some(uid) = &m.user {
                self.queue_user_lookup(uid);
            }
        }
    }

    fn resolve_unknown_user_ids(&mut self, ids: &[SlackUserId]) {
        for uid in ids {
            self.queue_user_lookup(uid);
        }
    }

    fn queue_user_lookup(&mut self, uid: &SlackUserId) {
        if self.users.contains_key(uid) || self.user_lookups.contains(uid) {
            return;
        }
        self.user_lookups.insert(uid.clone());
        let tx = self.tx.clone();
        let slack = self.slack.clone();
        let u = uid.clone();
        tokio::spawn(async move {
            if let Ok(info) = slack.user_info(&u).await {
                let _ = tx.send(AppEvent::UserLoaded(info));
            }
        });
    }

    pub fn display_name(&self, c: &Conv) -> String {
        let me = &self.slack.me;
        match &c.kind {
            ConvKind::Channel { private } => {
                if *private {
                    format!("🔒 {}", c.name)
                } else {
                    format!("#{}", c.name)
                }
            }
            ConvKind::Mpim => {
                if let Some(members) = self.conv_members.get(&c.id) {
                    let names: Vec<String> = members
                        .iter()
                        .filter(|u| *u != me)
                        .map(|u| {
                            self.users
                                .get(u)
                                .map(|info| info.display.clone())
                                .unwrap_or_else(|| u.0.clone())
                        })
                        .collect();
                    if !names.is_empty() {
                        return names.join(", ");
                    }
                }
                c.name
                    .strip_prefix("mpdm-")
                    .map(|s| {
                        let trimmed =
                            s.trim_end_matches(|ch: char| ch == '-' || ch.is_ascii_digit());
                        trimmed.replace("--", ", ")
                    })
                    .unwrap_or_else(|| c.name.clone())
            }
            ConvKind::Im { user_id } => {
                let uid = user_id.as_ref();
                let is_self = uid.map(|u| u == me).unwrap_or(false);
                let base = match uid.and_then(|u| self.users.get(u)) {
                    Some(u) => u.display.clone(),
                    None => match uid {
                        Some(u) => u.0.clone(),
                        None => c.name.clone(),
                    },
                };
                if is_self {
                    format!("{base} (you)")
                } else {
                    base
                }
            }
        }
    }

    pub fn author_label(&self, m: &Msg) -> String {
        if let Some(uid) = &m.user {
            if let Some(u) = self.users.get(uid) {
                return u.display.clone();
            }
            return uid.0.clone();
        }
        m.username.clone().unwrap_or_else(|| "system".to_string())
    }

    pub fn is_bot_author(&self, m: &Msg) -> bool {
        m.user
            .as_ref()
            .and_then(|uid| self.users.get(uid))
            .map(|u| u.is_bot)
            .unwrap_or(false)
            || m.subtype.is_some()
    }

    // -- Search ------------------------------------------------------------

    pub fn enter_sidebar_search(&mut self) {
        self.mode = Mode::SidebarSearch;
    }

    pub fn enter_chat_search(&mut self) {
        if self.selected_conv().is_some() {
            self.mode = Mode::ChatSearch;
        }
    }

    pub fn cancel_search(&mut self) {
        match self.mode {
            Mode::SidebarSearch => {
                self.sidebar_query.clear();
                self.mode = Mode::Normal;
            }
            Mode::ChatSearch => {
                self.chat_query.clear();
                self.chat_matches.clear();
                self.chat_match_idx = 0;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    pub fn accept_search(&mut self) {
        match self.mode {
            Mode::SidebarSearch => {
                self.mode = Mode::Normal;
                if self.selected_conv().is_none() {
                    self.jump_first_conv();
                }
            }
            Mode::ChatSearch => {
                self.mode = Mode::Normal;
                self.recompute_chat_matches();
                self.jump_to_current_chat_match();
            }
            _ => {}
        }
    }

    pub fn sidebar_search_input(&mut self, c: char) {
        self.sidebar_query.push(c);
        self.ensure_selection_visible();
    }

    pub fn sidebar_search_backspace(&mut self) {
        self.sidebar_query.pop();
        self.ensure_selection_visible();
    }

    pub fn chat_search_input(&mut self, c: char) {
        self.chat_query.push(c);
        self.recompute_chat_matches();
    }

    pub fn chat_search_backspace(&mut self) {
        self.chat_query.pop();
        self.recompute_chat_matches();
    }

    fn ensure_selection_visible(&mut self) {
        let rows = self.flat_visible();
        if rows.is_empty() {
            self.sidebar_state.select(None);
            return;
        }
        let cur = self.sidebar_state.selected();
        let valid = cur
            .and_then(|i| rows.get(i))
            .map(|r| matches!(r, VisibleRow::Conv(_)))
            .unwrap_or(false);
        if !valid {
            for (i, r) in rows.iter().enumerate() {
                if matches!(r, VisibleRow::Conv(_)) {
                    self.sidebar_state.select(Some(i));
                    self.on_selection_changed();
                    return;
                }
            }
            self.sidebar_state.select(None);
        }
    }

    fn recompute_chat_matches(&mut self) {
        self.chat_matches.clear();
        self.chat_match_idx = 0;
        if self.chat_query.is_empty() {
            return;
        }
        let q = self.chat_query.to_lowercase();
        let Some(c) = self.selected_conv() else {
            return;
        };
        let Some(msgs) = self.messages.get(&c.id) else {
            return;
        };
        for (i, m) in msgs.iter().enumerate() {
            if m.text.to_lowercase().contains(&q) {
                self.chat_matches.push(i);
            }
        }
        if !self.chat_matches.is_empty() {
            self.chat_match_idx = self.chat_matches.len() - 1;
        }
    }

    pub fn next_chat_match(&mut self, forward: bool) {
        if self.chat_matches.is_empty() {
            return;
        }
        let n = self.chat_matches.len();
        self.chat_match_idx = if forward {
            (self.chat_match_idx + 1) % n
        } else {
            (self.chat_match_idx + n - 1) % n
        };
        self.jump_to_current_chat_match();
    }

    fn jump_to_current_chat_match(&mut self) {
        let Some(c) = self.selected_conv() else {
            return;
        };
        let Some(msgs) = self.messages.get(&c.id) else {
            return;
        };
        let Some(&i) = self.chat_matches.get(self.chat_match_idx) else {
            return;
        };
        let from_end = (msgs.len().saturating_sub(1) - i) as u16;
        self.message_scroll = from_end;
    }

    pub fn current_match_index(&self) -> Option<usize> {
        self.chat_matches.get(self.chat_match_idx).copied()
    }

    pub fn error_text(&self) -> Option<&str> {
        self.error.as_ref().map(|(s, _)| s.as_str())
    }
}

pub enum VisibleRow {
    Header(Section, usize),
    Conv(usize),
}

pub enum ControlFlow {
    Continue,
    Quit,
}

fn spawn_unread_fetcher(
    slack: Arc<SlackContext>,
    tx: mpsc::UnboundedSender<AppEvent>,
    channels: Vec<SlackChannelId>,
) {
    // Channels need two calls each (conversations.info + a history probe to
    // count past `last_read`), so keep concurrency low to stay under Tier 3's
    // ~50/min ceiling for conversations.history.
    const CONCURRENCY: usize = 3;
    tokio::spawn(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut handles = Vec::with_capacity(channels.len());
        for ch in channels {
            let permit = sem.clone().acquire_owned().await;
            let Ok(permit) = permit else { break };
            let slack = slack.clone();
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                match slack.channel_unread(&ch).await {
                    Ok((count, latest_ts)) => {
                        let _ = tx.send(AppEvent::UnreadCountLoaded(ch, count, latest_ts));
                    }
                    Err(e) => {
                        debug_log(&format!("unread fetch {} failed: {e:#}", ch.0));
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

fn spawn_thread_replies_fetcher(
    slack: Arc<SlackContext>,
    tx: mpsc::UnboundedSender<AppEvent>,
    channel: SlackChannelId,
    parents: Vec<SlackTs>,
) {
    if parents.is_empty() {
        return;
    }
    // conversations.replies is on the same Tier 3 bucket as history; cap
    // ourselves to the recent few threads so a single conv load doesn't burn
    // the rate-limit budget on chatter from weeks ago.
    const MAX_PARENTS: usize = 10;
    const CONCURRENCY: usize = 2;
    tokio::spawn(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
        let mut handles = Vec::new();
        for parent_ts in parents.into_iter().rev().take(MAX_PARENTS) {
            let permit = sem.clone().acquire_owned().await;
            let Ok(permit) = permit else { break };
            let slack = slack.clone();
            let tx = tx.clone();
            let channel = channel.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                match slack.thread_replies(&channel, &parent_ts).await {
                    Ok(replies) => {
                        let _ = tx.send(AppEvent::ThreadReplies(channel, parent_ts, replies));
                    }
                    Err(e) => {
                        debug_log(&format!(
                            "thread_replies {}@{} failed: {e:#}",
                            channel.0, parent_ts.0
                        ));
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

fn debug_log(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/besty-term.log")
    {
        let _ = writeln!(f, "{}", line);
    }
}
