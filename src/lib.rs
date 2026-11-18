//! Multi-account Colab client. VM allocation requires an explicit session create operation.
pub mod accounts;
pub mod auth;
pub mod context;
pub mod credentials;
pub mod error;
pub mod models;
pub mod storage;

pub use error::{Error, Result};

pub mod colab_api;
pub mod sessions;

pub mod contents;

pub mod inventory;

pub mod jupyter;

pub mod execution;

pub mod history;

pub mod runs;

pub mod cli_compat;

pub mod automation;

pub mod installation;

pub mod ssh;

pub mod drive_credentials;
pub mod drivefs;

pub mod update;
pub mod utility;

pub mod websocket_transport;
