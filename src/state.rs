use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use slack_morphism::prelude::*;

use crate::slack::{Conv, ConvKind, UserInfo};

// On-disk snapshot. Each section is `#[serde(default)]` so a partial/older
// file still loads — we just refresh whatever's missing in the background.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub users: Vec<CachedUser>,
    #[serde(default)]
    pub convs: Vec<CachedConv>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedUser {
    pub id: String,
    pub display: String,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedConv {
    pub id: String,
    pub name: String,
    // "channel" | "private_channel" | "mpim" | "im"
    pub kind: String,
    #[serde(default)]
    pub im_user: Option<String>,
    #[serde(default)]
    pub is_member: bool,
    #[serde(default)]
    pub is_muted: bool,
    #[serde(default = "default_true")]
    pub is_open: bool,
    #[serde(default)]
    pub is_ext_shared: bool,
    #[serde(default)]
    pub is_dormant: bool,
    #[serde(default)]
    pub last_activity_ts: Option<String>,
}

fn default_true() -> bool {
    true
}

pub fn state_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support/besty-term/state.json"),
    )
}

pub fn load() -> Option<PersistedState> {
    let path = state_path()?;
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str::<PersistedState>(&raw).ok()
}

pub fn save(state: &PersistedState) -> Result<()> {
    let Some(path) = state_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create state dir")?;
    }
    let body = serde_json::to_string_pretty(state).context("serialize state")?;
    // Atomic write via tmp + rename — avoids a half-written file if we crash
    // mid-save (and crash on next startup trying to parse it).
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body).context("write state tmp")?;
    fs::rename(&tmp, &path).context("rename state tmp")?;
    Ok(())
}

pub fn rebuild_hidden(s: &PersistedState) -> HashSet<SlackChannelId> {
    s.hidden.iter().cloned().map(SlackChannelId).collect()
}

pub fn rebuild_users(s: &PersistedState) -> HashMap<SlackUserId, UserInfo> {
    s.users
        .iter()
        .map(|u| {
            let id = SlackUserId(u.id.clone());
            (
                id.clone(),
                UserInfo {
                    id,
                    display: u.display.clone(),
                    is_bot: u.is_bot,
                    deleted: u.deleted,
                },
            )
        })
        .collect()
}

pub fn rebuild_convs(s: &PersistedState) -> Vec<Conv> {
    s.convs
        .iter()
        .map(|c| {
            let kind = match c.kind.as_str() {
                "private_channel" => ConvKind::Channel { private: true },
                "mpim" => ConvKind::Mpim,
                "im" => ConvKind::Im {
                    user_id: c.im_user.clone().map(SlackUserId),
                },
                _ => ConvKind::Channel { private: false },
            };
            Conv {
                id: SlackChannelId(c.id.clone()),
                name: c.name.clone(),
                kind,
                is_member: c.is_member,
                is_disabled: false,
                is_muted: c.is_muted,
                is_open: c.is_open,
                is_ext_shared: c.is_ext_shared,
                is_dormant: c.is_dormant,
                last_activity_ts: c.last_activity_ts.clone().map(SlackTs),
            }
        })
        .collect()
}

pub fn snapshot(
    hidden: &HashSet<SlackChannelId>,
    users: &HashMap<SlackUserId, UserInfo>,
    convs: &[Conv],
) -> PersistedState {
    let hidden: Vec<String> = hidden.iter().map(|c| c.0.clone()).collect();
    let users: Vec<CachedUser> = users
        .values()
        .map(|u| CachedUser {
            id: u.id.0.clone(),
            display: u.display.clone(),
            is_bot: u.is_bot,
            deleted: u.deleted,
        })
        .collect();
    let convs: Vec<CachedConv> = convs
        .iter()
        .map(|c| {
            let (kind, im_user) = match &c.kind {
                ConvKind::Channel { private: false } => ("channel".to_string(), None),
                ConvKind::Channel { private: true } => ("private_channel".to_string(), None),
                ConvKind::Mpim => ("mpim".to_string(), None),
                ConvKind::Im { user_id } => {
                    ("im".to_string(), user_id.as_ref().map(|u| u.0.clone()))
                }
            };
            CachedConv {
                id: c.id.0.clone(),
                name: c.name.clone(),
                kind,
                im_user,
                is_member: c.is_member,
                is_muted: c.is_muted,
                is_open: c.is_open,
                is_ext_shared: c.is_ext_shared,
                is_dormant: c.is_dormant,
                last_activity_ts: c.last_activity_ts.as_ref().map(|t| t.0.clone()),
            }
        })
        .collect();
    PersistedState {
        hidden,
        users,
        convs,
    }
}
