use std::{collections::HashMap, env, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::connect_info::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn next_up_matches_observed_episode_history_and_enforces_user_and_catalog_policy() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_nextup_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .after_connect(move |connection, _| {
            let schema = connection_schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO \"{schema}\""))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .unwrap();
    common::apply_migrations(&pool).await.unwrap();
    let run_id = Uuid::new_v4();
    db::activate_run(&pool, run_id).await.unwrap();
    let owner = Uuid::new_v4();
    let peer = Uuid::new_v4();
    let administrator = Uuid::new_v4();
    for (id, name, admin) in [
        (owner, "nextup-owner", false),
        (peer, "nextup-peer", false),
        (administrator, "nextup-admin", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating) VALUES ($1,$2,$2,'unused-synthetic-hash',$3,TRUE,TRUE,50)")
            .bind(id).bind(name).bind(admin).execute(&pool).await.unwrap();
    }
    let library = Uuid::new_v4();
    let private_library = Uuid::new_v4();
    for (id, name) in [
        (library, "Original TV fixtures"),
        (private_library, "Private TV fixtures"),
    ] {
        db::insert_library(
            &pool,
            run_id,
            id,
            name,
            "tvshows",
            &[PathBuf::from("/media")],
            true,
        )
        .await
        .unwrap();
    }
    for user in [owner, peer] {
        sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
            .bind(user)
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (started, started_episodes) = series(
        &pool,
        library,
        "Started Show",
        &[(0, 1), (1, 1), (1, 2), (1, 3), (2, 1)],
        false,
    )
    .await;
    let (fresh, fresh_episodes) =
        series(&pool, library, "Fresh Show", &[(1, 1), (1, 2)], false).await;
    let (_, completed_episodes) =
        series(&pool, library, "Completed Show", &[(1, 1), (1, 2)], false).await;
    let (rated, rated_episodes) =
        series(&pool, library, "Rated Show", &[(1, 1), (1, 2)], false).await;
    let (hidden, hidden_episodes) =
        series(&pool, library, "Hidden Rated Show", &[(1, 1), (1, 2)], true).await;
    let (private_series, private_episodes) = series(
        &pool,
        private_library,
        "Private Show",
        &[(1, 1), (1, 2)],
        false,
    )
    .await;
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Next Up test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-nextup-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse().unwrap()],
        dlna: Default::default(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let mut tokens = HashMap::new();
    for user in [owner, peer, administrator] {
        let record = db::get_user(&pool, user).await.unwrap().unwrap();
        tokens.insert(
            user,
            auth::issue_token(&state, &record, "nextup-test", "fixture", &user.to_string())
                .await
                .unwrap()
                .token,
        );
    }
    let router = api::router(state.clone());
    let token = &tokens[&owner];
    assert_eq!(
        request(&router, "/Shows/NextUp", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_names(&router, "", token, &[], 0).await;
    favorite_series_sort(
        &state,
        &router,
        token,
        owner,
        [
            started_episodes[&(1, 1)],
            started_episodes[&(1, 2)],
            fresh_episodes[&(1, 1)],
            hidden_episodes[&(1, 1)],
            private_episodes[&(1, 1)],
        ],
    )
    .await;
    assert_names(
        &router,
        &format!("seriesId={fresh}"),
        token,
        &["Fresh Show S01E01"],
        1,
    )
    .await;
    for series in [hidden, private_series] {
        assert_eq!(
            request(
                &router,
                &format!("/Shows/NextUp?seriesId={series}"),
                Some(token)
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        request(
            &router,
            &format!("/Shows/NextUp?userId={peer}"),
            Some(token)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for (id, date) in [
        (started_episodes[&(1, 1)], "2026-10-01T00:00:00Z"),
        (completed_episodes[&(1, 1)], "2026-10-02T00:00:00Z"),
        (completed_episodes[&(1, 2)], "2026-10-02T00:01:00Z"),
        (rated_episodes[&(1, 1)], "2026-10-03T00:00:00Z"),
        (hidden_episodes[&(1, 1)], "2026-10-04T00:00:00Z"),
        (private_episodes[&(1, 1)], "2026-10-04T00:00:00Z"),
    ] {
        history(&pool, owner, id, true, 0, date).await;
    }
    assert_names(
        &router,
        "",
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        2,
    )
    .await;
    assert_names(
        &router,
        "limit=0",
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        2,
    )
    .await;
    assert_names(
        &router,
        "startIndex=1&limit=1",
        token,
        &["Started Show S01E02"],
        2,
    )
    .await;
    assert_names(&router, "startIndex=100&limit=1", token, &[], 2).await;
    assert_names(
        &router,
        "enableTotalRecordCount=false",
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        0,
    )
    .await;
    assert_names(
        &router,
        &format!("userId={owner}&limit=24&fields=PrimaryImageAspectRatio&fields=DateCreated&fields=Path&fields=MediaSourceCount&imageTypeLimit=1&enableImageTypes=Primary&enableImageTypes=Backdrop&enableImageTypes=Thumb&enableTotalRecordCount=false"),
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        0,
    ).await;
    assert_names(
        &router,
        &format!("userId={owner}&limit=24&fields=PrimaryImageAspectRatio&fields=DateCreated&fields=Path&fields=MediaSourceCount&imageTypeLimit=1&enableImageTypes=Primary&enableImageTypes=Backdrop&enableImageTypes=Thumb&nextUpDateCutoff=2025-10-05&enableTotalRecordCount=false&enableResumable=false&enableRewatching=false"),
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        0,
    ).await;
    // Encoded text inside an array value cannot introduce another user selector.
    assert_names(
        &router,
        &format!("fields=SortName&Fields=Overview%26userId%3D{peer}"),
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        2,
    )
    .await;
    assert_eq!(
        request(
            &router,
            &format!("/Shows/NextUp?fields=Name&fields=Overview&userId={owner}&UserId={peer}"),
            Some(token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST,
    );
    assert_names(
        &router,
        "nextUpDateCutoff=2026-10-02T00%3A00%3A00Z",
        token,
        &["Rated Show S01E02"],
        1,
    )
    .await;
    assert_names(
        &router,
        "nextUpDateCutoff=2026-10-02",
        token,
        &["Rated Show S01E02"],
        1,
    )
    .await;
    assert_names(
        &router,
        &format!("parentId={library}"),
        token,
        &["Rated Show S01E02", "Started Show S01E02"],
        2,
    )
    .await;
    assert_names(&router, &format!("parentId={started}"), token, &[], 0).await;
    assert_names(
        &router,
        &format!("seriesId={rated}"),
        token,
        &["Rated Show S01E02"],
        1,
    )
    .await;
    assert_names(&router, "", &tokens[&peer], &[], 0).await;
    assert_names(
        &router,
        &format!("userId={owner}"),
        &tokens[&administrator],
        &["Rated Show S01E02", "Started Show S01E02"],
        2,
    )
    .await;
    let projected = json(
        request(
            &router,
            "/Shows/NextUp?EnableImages=false&EnableUserData=false",
            Some(token),
        )
        .await,
    )
    .await;
    assert!(projected["Items"][0].get("UserData").is_none());
    assert!(projected["Items"][0].get("ImageTags").is_none());
    assert!(projected["Items"][0].get("Path").is_none());
    assert_eq!(projected["Items"][0]["SeriesId"], rated.to_string());
    assert_eq!(projected["Items"][0]["SeriesName"], "Rated Show");
    assert_eq!(projected["Items"][0]["SeasonName"], "Season 01");
    assert_eq!(projected["Items"][0]["IndexNumber"], 2);
    assert_eq!(projected["Items"][0]["ParentIndexNumber"], 1);
    history(
        &pool,
        owner,
        started_episodes[&(1, 3)],
        true,
        0,
        "2026-10-04T00:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "",
        token,
        &["Started Show S02E01", "Rated Show S01E02"],
        2,
    )
    .await;
    history(
        &pool,
        owner,
        started_episodes[&(2, 1)],
        true,
        0,
        "2026-10-04T00:01:00Z",
    )
    .await;
    assert_names(&router, "", token, &["Rated Show S01E02"], 1).await;
    history(
        &pool,
        owner,
        fresh_episodes[&(1, 1)],
        false,
        900_000_000,
        "2026-10-04T12:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "",
        token,
        &["Rated Show S01E02", "Fresh Show S01E01"],
        2,
    )
    .await;
    assert_names(
        &router,
        "enableResumable=false",
        token,
        &["Rated Show S01E02"],
        1,
    )
    .await;
    assert_names(
        &router,
        &format!("seriesId={fresh}&enableResumable=false"),
        token,
        &[],
        0,
    )
    .await;
    history(
        &pool,
        owner,
        fresh_episodes[&(1, 1)],
        false,
        0,
        "2026-10-04T12:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "nextUpDateCutoff=2026-10-04T00%3A00%3A00Z",
        token,
        &["Fresh Show S01E01"],
        1,
    )
    .await;
    history(
        &pool,
        owner,
        started_episodes[&(1, 2)],
        false,
        600_000_000,
        "2026-10-04T13:00:00Z",
    )
    .await;
    assert_names(&router, &format!("seriesId={started}"), token, &[], 0).await;
    history(
        &pool,
        owner,
        started_episodes[&(1, 1)],
        true,
        0,
        "2026-10-04T14:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "enableRewatching=true",
        token,
        &[
            "Started Show S01E03",
            "Rated Show S01E02",
            "Fresh Show S01E01",
        ],
        3,
    )
    .await;
    history(
        &pool,
        owner,
        started_episodes[&(0, 1)],
        true,
        0,
        "2026-10-04T15:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "enableRewatching=true",
        token,
        &[
            "Started Show S01E03",
            "Rated Show S01E02",
            "Fresh Show S01E01",
        ],
        3,
    )
    .await;
    history(
        &pool,
        owner,
        rated_episodes[&(1, 2)],
        false,
        600_000_000,
        "2026-10-04T16:00:00Z",
    )
    .await;
    assert_names(
        &router,
        "enableResumable=false",
        token,
        &["Fresh Show S01E01"],
        1,
    )
    .await;
    history(
        &pool,
        owner,
        started_episodes[&(1, 2)],
        true,
        0,
        "2026-10-04T13:30:00Z",
    )
    .await;
    assert_names(
        &router,
        "enableRewatching=true",
        token,
        &[
            "Started Show S01E02",
            "Rated Show S01E02",
            "Fresh Show S01E01",
        ],
        3,
    )
    .await;
    let mut bounded_state = state;
    let mut bounded_config = (*bounded_state.config).clone();
    bounded_config.max_page_size = 1;
    bounded_state.config = Arc::new(bounded_config);
    let bounded_router = api::router(bounded_state);
    assert_eq!(
        request(&bounded_router, "/Shows/NextUp?limit=0", Some(token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_names(&bounded_router, "limit=1", token, &["Rated Show S01E02"], 2).await;
    for options in [
        "Limit=-1",
        "StartIndex=-1",
        "StartIndex=2147483648",
        "nextUpDateCutoff=invalid",
        "nextUpDateCutoff=2026-02-30",
        "nextUpDateCutoff=2026-1-2",
        "imageTypeLimit=-1",
    ] {
        assert_eq!(
            request(&router, &format!("/Shows/NextUp?{options}"), Some(token))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    // A partial rewatch can coexist with the normal next unwatched episode.
    sqlx::query("UPDATE user_item_data SET played=FALSE,playback_position_ticks=0,last_played_at=NULL WHERE user_id=$1 AND item_id=$2")
        .bind(owner).bind(started_episodes[&(2, 1)]).execute(&pool).await.unwrap();
    assert_names(
        &router,
        "",
        token,
        &[
            "Started Show S02E01",
            "Rated Show S01E02",
            "Fresh Show S01E01",
        ],
        3,
    )
    .await;
    assert_names(
        &router,
        "enableRewatching=true",
        token,
        &[
            "Started Show S01E02",
            "Started Show S02E01",
            "Rated Show S01E02",
            "Fresh Show S01E01",
        ],
        4,
    )
    .await;
    assert_names(
        &router,
        &format!("seriesId={started}&enableRewatching=true"),
        token,
        &["Started Show S01E02", "Started Show S02E01"],
        2,
    )
    .await;
    assert_names(
        &router,
        "enableRewatching=true&startIndex=1&limit=2",
        token,
        &["Started Show S02E01", "Rated Show S01E02"],
        4,
    )
    .await;
    // Favorite-only history must not start a show without an explicit seriesId.
    sqlx::query("UPDATE user_item_data SET played=FALSE,playback_position_ticks=0,last_played_at=NULL,is_favorite=TRUE WHERE user_id=$1 AND item_id=$2")
        .bind(owner).bind(fresh_episodes[&(1, 1)]).execute(&pool).await.unwrap();
    assert_names(
        &router,
        "",
        token,
        &["Started Show S02E01", "Rated Show S01E02"],
        2,
    )
    .await;
    assert_names(
        &router,
        &format!("seriesId={fresh}"),
        token,
        &["Fresh Show S01E01"],
        1,
    )
    .await;
    // Policy changes are applied by the same live query before ranking/counting.
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    assert_names(&router, "", token, &[], 0).await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn item(
    pool: &PgPool,
    library: Uuid,
    parent: Option<Uuid>,
    name: &str,
    kind: &str,
    path: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,3000230000)")
        .bind(id).bind(library).bind(parent).bind(name).bind(name.to_lowercase()).bind(kind).bind(path).bind(db::path_hash(path)).execute(pool).await.unwrap();
    id
}

async fn series(
    pool: &PgPool,
    library: Uuid,
    name: &str,
    numbers: &[(i32, i32)],
    rated: bool,
) -> (Uuid, HashMap<(i32, i32), Uuid>) {
    let folder = format!("/media/{library}/{name}");
    let series = item(pool, library, None, name, "Series", &folder).await;
    if rated {
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo','US-MPAA-v1',90)").bind(series).execute(pool).await.unwrap();
    }
    let mut seasons = HashMap::new();
    let mut episodes = HashMap::new();
    for &(season, episode) in numbers {
        let season_id = match seasons.get(&season) {
            Some(id) => *id,
            None => {
                let id = item(
                    pool,
                    library,
                    Some(series),
                    &format!("Season {season:02}"),
                    "Season",
                    &format!("{folder}/Season {season:02}"),
                )
                .await;
                seasons.insert(season, id);
                id
            }
        };
        let title = format!("{name} S{season:02}E{episode:02}");
        let id = item(
            pool,
            library,
            Some(season_id),
            &title,
            "Episode",
            &format!("{folder}/Season {season:02}/{title}.mkv"),
        )
        .await;
        episodes.insert((season, episode), id);
    }
    (series, episodes)
}

async fn favorite_series_sort(
    state: &AppState,
    router: &Router,
    token: &str,
    user: Uuid,
    items: [Uuid; 5],
) {
    let original: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id,sort_name FROM items WHERE id=ANY($1)")
            .bind(items.to_vec())
            .fetch_all(&state.db)
            .await
            .unwrap();
    // The general catalogue's policy applies each item's classification;
    // Next Up additionally checks the enclosing series and season. Classify
    // this leaf explicitly for the catalogue sorting and filtering checks.
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES($1,'local-nfo','US-MPAA-v1',90)")
        .bind(items[3]).execute(&state.db).await.unwrap();
    for id in items {
        db::set_item_favorite(&state.db, state.run_id, user, id, true)
            .await
            .unwrap();
    }
    // Episode titles deliberately disagree with series order, so accepting
    // the option without applying its primary key cannot pass these checks.
    for (id, name) in [
        (items[0], "a title"),
        (items[1], "z title"),
        (items[2], "m title"),
    ] {
        sqlx::query("UPDATE items SET sort_name=$2 WHERE id=$1")
            .bind(id)
            .bind(name)
            .execute(&state.db)
            .await
            .unwrap();
    }
    for (fields, order, expected) in [
        (
            "SeriesSortName,SortName",
            "Ascending",
            [items[2], items[0], items[1]],
        ),
        (
            "SeriesSortName,SortName",
            "Descending",
            [items[1], items[0], items[2]],
        ),
        (
            "SeriesSortName,SortName",
            "Descending,Ascending",
            [items[0], items[1], items[2]],
        ),
        (
            "seriessortname",
            "Descending",
            [items[0], items[1], items[2]],
        ),
    ] {
        let path = format!(
            "/Users/{user}/Items?SortBy={fields}&SortOrder={order}&Filters=IsFavorite&Recursive=true&Fields=PrimaryImageAspectRatio&CollapseBoxSetItems=false&ExcludeLocationTypes=Virtual&EnableTotalRecordCount=false&Limit=20&IncludeItemTypes=Episode"
        );
        let result = json(request(router, &path, Some(token)).await).await;
        let actual = result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{path}");
        // The ordinary catalogue currently reports page size when full
        // counting is disabled; the reference's episode count of zero is
        // a separate qualified difference from these ordering checks.
        assert_eq!(result["TotalRecordCount"], 3);
    }
    let page = json(request(router, "/Items?SortBy=SeriesSortName,SortName&Filters=IsFavorite&Recursive=true&IncludeItemTypes=Episode&Limit=1&StartIndex=1", Some(token)).await).await;
    assert_eq!(page["TotalRecordCount"], 3);
    assert_eq!(page["Items"][0]["Id"], items[0].to_string());
    for kind in [
        "Movie",
        "Series",
        "Season",
        "Audio",
        "MusicAlbum",
        "MusicArtist",
        "Photo",
    ] {
        let result = json(request(router, &format!("/Items?SortBy=SeriesSortName,SortName&Filters=IsFavorite&Recursive=true&IncludeItemTypes={kind}&Limit=20"), Some(token)).await).await;
        assert!(result["Items"].as_array().unwrap().is_empty(), "{kind}");
    }
    for (id, name) in original {
        sqlx::query("UPDATE items SET sort_name=$2 WHERE id=$1")
            .bind(id)
            .bind(name)
            .execute(&state.db)
            .await
            .unwrap();
        db::set_item_favorite(&state.db, state.run_id, user, id, false)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(items[3])
        .execute(&state.db)
        .await
        .unwrap();
}

async fn history(pool: &PgPool, user: Uuid, item: Uuid, played: bool, position: i64, date: &str) {
    sqlx::query("INSERT INTO user_item_data(user_id,item_id,played,playback_position_ticks,last_played_at) VALUES ($1,$2,$3,$4,$5::text::timestamptz) ON CONFLICT(user_id,item_id) DO UPDATE SET played=EXCLUDED.played,playback_position_ticks=EXCLUDED.playback_position_ticks,last_played_at=EXCLUDED.last_played_at")
        .bind(user).bind(item).bind(played).bind(position).bind(date).execute(pool).await.unwrap();
}

async fn request(router: &Router, path: &str, token: Option<&str>) -> Response {
    let mut request = Request::builder().uri(path).extension(ConnectInfo(
        "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn json(response: Response) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

async fn assert_names(router: &Router, query: &str, token: &str, names: &[&str], count: i64) {
    let value = json(request(router, &format!("/Shows/NextUp?{query}"), Some(token)).await).await;
    let actual = value["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, names, "{query}");
    assert_eq!(value["TotalRecordCount"], count, "{query}");
}
