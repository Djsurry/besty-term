use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Deserialize;
use slack_morphism::prelude::*;
use slack_morphism::SLACK_HTTP_EMPTY_GET_PARAMS;

pub type SlackClientHandle = Arc<SlackHyperClient>;

#[derive(Debug, Clone)]
pub enum ConvKind {
    Channel { private: bool },
    Mpim,
    Im { user_id: Option<SlackUserId> },
}

#[derive(Debug, Clone)]
pub struct Conv {
    pub id: SlackChannelId,
    pub name: String,
    pub kind: ConvKind,
    pub is_member: bool,
    pub is_disabled: bool,
    pub is_muted: bool,
    // Slack's sidebar hides DMs/MPIMs the user has "closed". `is_open` is
    // returned only for IM/MPIM types; channels are treated as always open.
    pub is_open: bool,
    // Slack Connect (shared with an external workspace). Used to group these
    // under an "External" section, mirroring Slack's own sidebar.
    pub is_ext_shared: bool,
    // Slack's signal that a conversation should be hidden from the sidebar
    // due to inactivity. Set on `properties.is_dormant` in conversations.list.
    pub is_dormant: bool,
    // Timestamp of the most recent message we know about. Sourced from
    // `latest.ts` in conversations.list and bumped as new messages arrive.
    pub last_activity_ts: Option<SlackTs>,
}

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub id: SlackUserId,
    pub display: String,
    pub is_bot: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub ts: SlackTs,
    pub user: Option<SlackUserId>,
    pub username: Option<String>,
    pub text: String,
    pub subtype: Option<String>,
    // For replies, the parent message's ts. For parents/standalone msgs, None.
    // (Slack sets thread_ts == ts on a thread root once it has replies, but
    // we normalize that to None so "is this a reply?" is a clean check.)
    pub thread_ts: Option<SlackTs>,
    // Number of replies if this message is a thread parent; 0 otherwise.
    pub reply_count: u32,
    // True for messages we inserted optimistically before chat.postMessage
    // returned. Swapped to false once the real ts comes back (or the entry is
    // removed if the send failed).
    pub pending: bool,
}

impl Msg {
    fn from_raw(m: RawMessage) -> Self {
        let ts = SlackTs(m.ts.clone());
        let thread_ts = m
            .thread_ts
            .filter(|t| !t.is_empty() && t != &m.ts)
            .map(SlackTs);
        Self {
            ts,
            user: m.user.map(SlackUserId),
            username: m.username,
            text: m.text.unwrap_or_default(),
            subtype: m.subtype,
            thread_ts,
            reply_count: m.reply_count.unwrap_or(0),
            pending: false,
        }
    }

    pub fn from_event(e: &SlackMessageEvent) -> Option<Self> {
        let ts = e.origin.ts.clone();
        let thread_ts = e
            .origin
            .thread_ts
            .clone()
            .filter(|t| t != &ts);
        Some(Self {
            ts,
            user: e.sender.user.clone(),
            username: e.sender.username.clone(),
            text: e
                .content
                .as_ref()
                .and_then(|c| c.text.clone())
                .unwrap_or_default(),
            subtype: e.subtype.as_ref().map(event_subtype_str),
            thread_ts,
            reply_count: 0,
            pending: false,
        })
    }

    pub fn is_thread_reply(&self) -> bool {
        self.thread_ts.is_some()
    }

    pub fn local_time(&self) -> Option<DateTime<Local>> {
        self.ts.to_date_time_opt().map(|dt| dt.with_timezone(&Local))
    }
}

fn event_subtype_str(s: &SlackMessageEventType) -> String {
    // SlackMessageEventType serializes as a bare string literal — strip the
    // surrounding quotes to recover the API name (e.g. "bot_message").
    serde_json::to_string(s)
        .ok()
        .and_then(|raw| raw.trim_matches('"').to_string().into())
        .filter(|s: &String| !s.is_empty())
        .unwrap_or_else(|| "event".to_string())
}

pub struct SlackContext {
    pub client: SlackClientHandle,
    pub token: SlackApiToken,
    pub me: SlackUserId,
    pub team: String,
}

impl SlackContext {
    pub async fn connect(user_token: String) -> Result<Self> {
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
        let token = SlackApiToken::new(user_token.into());
        let session = client.open_session(&token);
        let auth = session
            .auth_test()
            .await
            .context("auth.test failed — is SLACK_USER_TOKEN valid?")?;
        Ok(Self {
            client,
            token,
            me: auth.user_id,
            team: auth.team,
        })
    }

    pub async fn list_users(&self) -> Result<HashMap<SlackUserId, UserInfo>> {
        let session = self.client.open_session(&self.token);
        let mut cursor: Option<SlackCursorId> = None;
        let mut out = HashMap::new();
        loop {
            let req = SlackApiUsersListRequest::new()
                .with_limit(500)
                .opt_cursor(cursor.clone());
            let resp = session
                .users_list(&req)
                .await
                .context("users.list failed")?;
            for u in resp.members {
                let info = user_to_info(&u);
                out.insert(info.id.clone(), info);
            }
            cursor = resp
                .response_metadata
                .as_ref()
                .and_then(|m| m.next_cursor.clone())
                .filter(|c| !c.0.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn list_conversations(&self) -> Result<Vec<Conv>> {
        let session = self.client.open_session(&self.token);
        let mut cursor = String::new();
        let mut out = Vec::new();
        let types = "public_channel,private_channel,im,mpim".to_string();
        let limit = "200".to_string();
        let exclude = "true".to_string();
        loop {
            let mut params: Vec<(&str, Option<&String>)> = vec![
                ("types", Some(&types)),
                ("limit", Some(&limit)),
                ("exclude_archived", Some(&exclude)),
            ];
            if !cursor.is_empty() {
                params.push(("cursor", Some(&cursor)));
            }
            let resp: RawConversationsListResponse = session
                .http_session_api
                .http_get("conversations.list", &params, None)
                .await
                .context("conversations.list failed")?;
            for c in resp.channels {
                out.push(raw_channel_to_conv(c));
            }
            cursor = resp
                .response_metadata
                .and_then(|m| m.next_cursor)
                .unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    pub async fn im_partner(&self, channel: &SlackChannelId) -> Result<Option<SlackUserId>> {
        Ok(self
            .conversation_members(channel)
            .await?
            .into_iter()
            .find(|u| u != &self.me))
    }

    pub async fn conversation_members(
        &self,
        channel: &SlackChannelId,
    ) -> Result<Vec<SlackUserId>> {
        let session = self.client.open_session(&self.token);
        let req = SlackApiConversationsMembersRequest::new()
            .with_channel(channel.clone())
            .with_limit(50);
        let resp = session
            .conversations_members(&req)
            .await
            .context("conversations.members failed")?;
        Ok(resp.members)
    }

    pub async fn history(&self, channel: &SlackChannelId, limit: u16) -> Result<Vec<Msg>> {
        let resp = self.raw_history(channel, None, limit).await?;
        let mut msgs: Vec<Msg> = resp.messages.into_iter().map(Msg::from_raw).collect();
        msgs.reverse();
        Ok(msgs)
    }

    pub async fn history_after(
        &self,
        channel: &SlackChannelId,
        oldest: &SlackTs,
        limit: u16,
    ) -> Result<Vec<Msg>> {
        let resp = self.raw_history(channel, Some(oldest), limit).await?;
        let mut msgs: Vec<Msg> = resp
            .messages
            .into_iter()
            .filter(|m| m.ts != oldest.0)
            .map(Msg::from_raw)
            .collect();
        msgs.reverse();
        Ok(msgs)
    }

    async fn raw_history(
        &self,
        channel: &SlackChannelId,
        oldest: Option<&SlackTs>,
        limit: u16,
    ) -> Result<RawHistoryResponse> {
        let limit_s = limit.to_string();
        let oldest_s = oldest.map(|t| t.0.clone());
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3u32 {
            let session = self.client.open_session(&self.token);
            let mut params: Vec<(&str, Option<&String>)> = vec![
                ("channel", Some(&channel.0)),
                ("limit", Some(&limit_s)),
            ];
            if let Some(o) = oldest_s.as_ref() {
                params.push(("oldest", Some(o)));
            }
            let result: Result<RawHistoryResponse, _> = session
                .http_session_api
                .http_get("conversations.history", &params, None)
                .await;
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let msg = format!("{e:#}");
                    let rate_limited = msg.contains("ratelimited")
                        || msg.contains("rate_limited")
                        || msg.contains("429");
                    if rate_limited && attempt < 2 {
                        let backoff = std::time::Duration::from_millis(
                            1500u64 * (1u64 << attempt),
                        );
                        tokio::time::sleep(backoff).await;
                        last_err = Some(anyhow::anyhow!(msg));
                        continue;
                    }
                    return Err(anyhow::Error::new(e).context("conversations.history failed"));
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("conversations.history: exhausted retries")))
    }

    pub async fn thread_replies(
        &self,
        channel: &SlackChannelId,
        parent_ts: &SlackTs,
    ) -> Result<Vec<Msg>> {
        let session = self.client.open_session(&self.token);
        let limit = "100".to_string();
        let params: Vec<(&str, Option<&String>)> = vec![
            ("channel", Some(&channel.0)),
            ("ts", Some(&parent_ts.0)),
            ("limit", Some(&limit)),
        ];
        let resp: RawHistoryResponse = session
            .http_session_api
            .http_get("conversations.replies", &params, None)
            .await
            .context("conversations.replies failed")?;
        // conversations.replies returns the parent at index 0 followed by its
        // replies in chronological order. Return everything so callers that
        // don't already have the parent get it for free — the caller's merge
        // logic dedupes on ts.
        let mut msgs: Vec<Msg> = resp.messages.into_iter().map(Msg::from_raw).collect();
        msgs.sort_by(|a, b| a.ts.0.cmp(&b.ts.0));
        Ok(msgs)
    }

    pub async fn open_self_dm(&self) -> Result<SlackChannelId> {
        let session = self.client.open_session(&self.token);
        let req = SlackApiConversationsOpenRequest::new().with_users(vec![self.me.clone()]);
        let resp = session
            .conversations_open(&req)
            .await
            .context("conversations.open (self) failed")?;
        Ok(resp.channel.id)
    }

    pub async fn user_info(&self, user: &SlackUserId) -> Result<UserInfo> {
        let session = self.client.open_session(&self.token);
        let resp = session
            .users_info(&SlackApiUsersInfoRequest::new(user.clone()))
            .await
            .context("users.info failed")?;
        Ok(user_to_info(&resp.user))
    }

    pub async fn channel_unread(
        &self,
        channel: &SlackChannelId,
    ) -> Result<(u32, Option<SlackTs>)> {
        let session = self.client.open_session(&self.token);
        let params: Vec<(&str, Option<&String>)> = vec![("channel", Some(&channel.0))];
        let raw: serde_json::Value = session
            .http_session_api
            .http_get("conversations.info", &params, None)
            .await
            .context("conversations.info failed")?;
        let ch_obj = raw.get("channel");
        // `latest.ts` is reliably present on conversations.info responses for
        // IM/MPIM channels (and on channels with any history). Pull it so the
        // sidebar can sort/filter DMs by recency without a second probe.
        let latest_ts = ch_obj
            .and_then(|c| c.get("latest"))
            .and_then(|l| l.get("ts"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| SlackTs(s.to_string()));
        // Prefer Slack's own count when present (DMs/MPIMs with appropriate scopes).
        if let Some(n) = ch_obj
            .and_then(|c| c.get("unread_count_display"))
            .and_then(|v| v.as_u64())
        {
            return Ok((n as u32, latest_ts));
        }
        // Fall back: count messages newer than `last_read` ourselves. Channels
        // don't include unread_count_display unless the legacy `read` scope is
        // granted, but `last_read` is present on every member channel.
        let last_read = ch_obj
            .and_then(|c| c.get("last_read"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if last_read.is_empty() || last_read == "0000000000.000000" {
            return Ok((0, latest_ts));
        }
        let oldest = SlackTs(last_read.to_string());
        let msgs = self.history_after(channel, &oldest, 50).await?;
        // Slack's own unread badge ignores system messages (channel_join/leave,
        // topic/purpose changes, pin/unpin, bot add/remove, etc.). Count only
        // real human/bot messages so we match Slack's UI.
        let n = msgs
            .iter()
            .filter(|m| match m.subtype.as_deref() {
                None | Some("me_message") | Some("thread_broadcast") | Some("file_share") => true,
                _ => false,
            })
            .count();
        Ok((n as u32, latest_ts))
    }

    pub async fn list_muted_channels(&self) -> Result<HashSet<SlackChannelId>> {
        let session = self.client.open_session(&self.token);
        let resp: UsersPrefsGetResponse = session
            .http_session_api
            .http_get("users.prefs.get", &SLACK_HTTP_EMPTY_GET_PARAMS.clone(), None)
            .await
            .context("users.prefs.get failed")?;
        let raw = resp.prefs.muted_channels.unwrap_or_default();
        let muted: HashSet<SlackChannelId> = raw
            .split(',')
            .filter(|s: &&str| !s.is_empty())
            .map(|s: &str| SlackChannelId(s.to_string()))
            .collect();
        Ok(muted)
    }

    pub async fn mark_read(&self, channel: &SlackChannelId, ts: &SlackTs) -> Result<()> {
        let session = self.client.open_session(&self.token);
        #[derive(serde::Serialize)]
        struct Req<'a> {
            channel: &'a str,
            ts: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct Resp {}
        let req = Req {
            channel: &channel.0,
            ts: &ts.0,
        };
        let _: Resp = session
            .http_session_api
            .http_post("conversations.mark", &req, None)
            .await
            .context("conversations.mark failed")?;
        Ok(())
    }

    pub async fn post_message(&self, channel: &SlackChannelId, text: &str) -> Result<SlackTs> {
        let session = self.client.open_session(&self.token);
        let content = SlackMessageContent::new().with_text(text.to_string());
        let req = SlackApiChatPostMessageRequest::new(channel.clone(), content);
        let resp = session
            .chat_post_message(&req)
            .await
            .context("chat.postMessage failed")?;
        Ok(resp.ts)
    }
}

#[derive(Deserialize)]
struct UsersPrefsGetResponse {
    prefs: UsersPrefs,
}

#[derive(Deserialize)]
struct UsersPrefs {
    #[serde(default)]
    muted_channels: Option<String>,
}

#[derive(Deserialize)]
struct RawHistoryResponse {
    #[serde(default)]
    messages: Vec<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    ts: String,
    user: Option<String>,
    username: Option<String>,
    #[serde(default)]
    text: Option<String>,
    subtype: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    reply_count: Option<u32>,
}

pub fn user_to_info(u: &SlackUser) -> UserInfo {
    let nonempty = |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty());
    let from_profile = |f: fn(&SlackUserProfile) -> Option<String>| {
        u.profile.as_ref().and_then(|p| nonempty(&f(p)))
    };
    let first_last = u.profile.as_ref().and_then(|p| {
        let first = p.first_name.as_deref().unwrap_or("").trim();
        let last = p.last_name.as_deref().unwrap_or("").trim();
        let combined = format!("{first} {last}");
        let combined = combined.trim().to_string();
        (!combined.is_empty()).then_some(combined)
    });
    // Match Slack desktop's preference: chosen display name → real name →
    // first/last from the profile → finally the username handle.
    let display = from_profile(|p| p.display_name.clone())
        .or_else(|| from_profile(|p| p.real_name.clone()))
        .or_else(|| nonempty(&u.real_name))
        .or(first_last)
        .or_else(|| from_profile(|p| p.first_name.clone()))
        .or_else(|| nonempty(&u.name))
        .unwrap_or_else(|| u.id.0.clone());
    let is_bot = u.flags.is_bot.unwrap_or(false);
    let deleted = u.deleted.unwrap_or(false);
    UserInfo {
        id: u.id.clone(),
        display,
        is_bot,
        deleted,
    }
}

#[derive(Deserialize)]
struct RawConversationsListResponse {
    #[serde(default)]
    channels: Vec<RawChannel>,
    response_metadata: Option<RawCursorMeta>,
}

#[derive(Deserialize)]
struct RawCursorMeta {
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct RawChannel {
    id: String,
    name: Option<String>,
    #[serde(default)]
    is_im: bool,
    #[serde(default)]
    is_mpim: bool,
    #[serde(default)]
    is_private: bool,
    #[serde(default)]
    is_member: bool,
    #[serde(default)]
    is_archived: bool,
    #[serde(default)]
    is_user_deleted: bool,
    #[serde(default)]
    is_ext_shared: bool,
    #[serde(default)]
    properties: Option<RawChannelProperties>,
    // Slack's `conversations.list` returns `is_open` as null for IMs/MPIMs
    // rather than a real bool, so accept Option<bool>. Treat missing/null as
    // open and only hide when Slack explicitly says closed.
    #[serde(default)]
    is_open: Option<bool>,
}

#[derive(Deserialize)]
struct RawChannelProperties {
    #[serde(default)]
    is_dormant: bool,
}

fn raw_channel_to_conv(c: RawChannel) -> Conv {
    let kind = if c.is_im {
        ConvKind::Im { user_id: None }
    } else if c.is_mpim {
        ConvKind::Mpim
    } else {
        ConvKind::Channel {
            private: c.is_private,
        }
    };
    let is_dm_kind = c.is_im || c.is_mpim;
    let name = c.name.unwrap_or_else(|| c.id.clone());
    let is_dormant = c.properties.as_ref().map(|p| p.is_dormant).unwrap_or(false);
    let last_activity_ts: Option<SlackTs> = None;
    Conv {
        id: SlackChannelId(c.id),
        name,
        kind,
        is_member: c.is_member || is_dm_kind,
        is_disabled: c.is_archived || c.is_user_deleted,
        is_muted: false,
        is_open: if is_dm_kind { c.is_open.unwrap_or(true) } else { true },
        is_ext_shared: c.is_ext_shared,
        is_dormant,
        last_activity_ts,
    }
}
