pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod library;
pub mod state;

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
