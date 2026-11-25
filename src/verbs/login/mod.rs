//! Login: the browser OAuth (PKCE) flow, setup tokens and the Claude Code and
//! Codex imports, one concern per child module. Each child reads this module's
//! imports through `use super::*`, and the `pub use` lines keep every
//! `crate::verbs::login::...` path resolving. The token client they mint and refresh
//! with is [`crate::clients::oauth`].

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

use crate::base::config::{self, Account, Config};
use crate::base::log_err::LogErr as _;
use crate::clients::oauth::{
    CLIENT_ID, ExchangeIdentity, OAUTH_ACCEPT, OAUTH_USER_AGENT, Profile, RefreshResponse,
    StoredClaudeIdentity, TOKEN_ENDPOINT, Tokens, expires_at_from, fetch_profile,
    normalize_expires_at,
};
use crate::stores::singleton;

mod account;
mod admission;
mod browser;
mod claude_import;
pub mod codex_cli;
mod codex_import;
mod login_ui;
mod persist;
mod setup_token;
#[cfg(test)]
mod tests;

pub use account::*;
pub(crate) use admission::*;
pub use browser::*;
pub use claude_import::*;
pub use codex_import::*;
pub use login_ui::*;
pub use persist::*;
pub use setup_token::*;
