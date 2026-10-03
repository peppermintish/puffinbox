use sqlx::PgPool;

const MIGRATIONS_BEFORE_SERIES_DEDUP: [&str; 14] = [
    include_str!("../../migrations/0001_initial.sql"),
    include_str!("../../migrations/0002_one_active_playback_session_per_device.sql"),
    include_str!("../../migrations/0003_library_root_identity.sql"),
    include_str!("../../migrations/0004_playback_server_run.sql"),
    include_str!("../../migrations/0005_metadata_plugins.sql"),
    include_str!("../../migrations/0006_live_tv.sql"),
    include_str!("../../migrations/0007_offline_packages.sql"),
    include_str!("../../migrations/0008_dlna_pairings.sql"),
    include_str!("../../migrations/0009_live_tv_program_ratings.sql"),
    include_str!("../../migrations/0010_dlna_catalog_revision.sql"),
    include_str!("../../migrations/0011_live_tv_timer_display_name.sql"),
    include_str!("../../migrations/0012_live_tv_series_timers.sql"),
    include_str!("../../migrations/0013_live_tv_recording_policy_lookup.sql"),
    include_str!("../../migrations/0014_live_tv_user_policy.sql"),
];

pub async fn apply_migrations_through_0014(pool: &PgPool) -> Result<(), sqlx::Error> {
    for migration in MIGRATIONS_BEFORE_SERIES_DEDUP {
        sqlx::raw_sql(migration).execute(pool).await?;
    }
    Ok(())
}

pub async fn apply_migrations_through_0018(pool: &PgPool) -> Result<(), sqlx::Error> {
    apply_migrations_through_0014(pool).await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0015_live_tv_series_airing_dedup.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0016_scoped_media_access_tokens.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!("../../migrations/0017_user_playlists.sql"))
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0018_remote_access_opt_in.sql"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn apply_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    apply_migrations_through_0018(pool).await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0019_live_tv_source_lifecycle.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0020_session_capabilities.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!("../../migrations/0021_user_preferences.sql"))
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!("../../migrations/0022_playlist_users.sql"))
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0023_playback_stop_reports.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!("../../migrations/0024_studio_favorites.sql"))
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/0025_embedded_audio_metadata.sql"
    ))
    .execute(pool)
    .await?;
    Ok(())
}
