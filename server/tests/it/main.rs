//! 集成测试：需要本地 Postgres（docker compose up db）。测试库 chat_test 自动创建。
//! 运行：cargo test --manifest-path server/Cargo.toml

use chat_server::build_router;
use chat_server::config::Config;
use chat_server::state::AppState;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use std::time::Duration as Dur;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMsg;

mod admin;
mod announcements;
mod attachments;
mod avatar;
mod backup;
mod basics;
mod bots;
mod common;
mod conv_prefs;
mod forward;
mod invites;
mod messages;
mod polls;
mod push;
mod regressions;
mod search;
mod sessions;
mod voice;

use avatar::*;
use backup::*;
use common::*;
use conv_prefs::*;
use messages::*;
use regressions::*;
