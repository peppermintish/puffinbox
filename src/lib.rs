pub mod api;
pub mod auth;
pub(crate) mod catalog_filters;
pub(crate) mod catalog_navigation;
pub mod client_connection;
pub mod config;
pub mod db;
pub mod error;
pub mod library;
pub mod state;
pub mod user_settings;
pub(crate) mod websocket;

// Media endpoints are implemented in their own module so each API can share the
// same authentication and library authorization policy.
pub mod media_features;
pub mod metadata;
pub mod offline;
pub mod playlists;
pub mod plugins;

pub use config::Config;
pub use error::ApiError;
pub use state::AppState;
