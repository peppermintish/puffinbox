use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row, Transaction, types::Json};
use uuid::Uuid;

use super::{UserRecord, path_hash, push_rating_visibility_filters};

/// Register bounded credit names after the metadata write and its source fence.
/// Existing physical artist names keep their identities. Generated identities
/// remain stored when inactive; readers require a visible current credit source.
pub(crate) async fn register_metadata_artists(
    tx: &mut Transaction<'_, Postgres>,
    item_id: Uuid,
    provider: &str,
) -> Result<(), sqlx::Error> {
    if !matches!(provider, "local-nfo" | "embedded-audio") {
        return Ok(());
    }
    let row = sqlx::query("SELECT i.library_id,m.metadata_json FROM items i JOIN item_metadata m ON m.item_id=i.id AND m.provider_key=$2 WHERE i.id=$1 AND i.item_type='Audio' FOR UPDATE OF i")
        .bind(item_id).bind(provider).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let library_id: Uuid = row.try_get("library_id")?;
    let metadata: Value = row.try_get("metadata_json")?;
    let mut credits = Vec::new();
    for role in ["artists", "albumArtists"] {
        if let Some(names) = metadata[role].as_array() {
            for name in names.iter().take(32).filter_map(Value::as_str) {
                if !name.trim_matches(' ').is_empty()
                    && name.len() <= 512
                    && !name.chars().any(char::is_control)
                {
                    credits.push((role, name.to_owned()));
                }
            }
        }
    }
    sqlx::query("DELETE FROM music_tag_artist_sources WHERE item_id=$1 AND provider_key=$2")
        .bind(item_id)
        .bind(provider)
        .execute(&mut **tx)
        .await?;
    // Use the database's normalization and a common lock order across tracks.
    let names = sqlx::query("SELECT name,btrim(name) AS display_name FROM (SELECT DISTINCT name FROM unnest($1::text[]) AS input(name)) names ORDER BY lower(btrim(name)) COLLATE \"C\",name COLLATE \"C\"")
        .bind(credits.iter().map(|(_,name)| name.clone()).collect::<Vec<_>>())
        .fetch_all(&mut **tx).await?;
    for row in names {
        let source_name: String = row.try_get("name")?;
        let name: String = row.try_get("display_name")?;
        let physical: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items i WHERE i.library_id=$1 AND i.item_type='MusicArtist' AND lower(i.name)=lower($2) AND NOT EXISTS(SELECT 1 FROM music_tag_artists tag WHERE tag.artist_id=i.id))")
            .bind(library_id).bind(&name).fetch_one(&mut **tx).await?;
        if physical {
            continue;
        }
        let artist = sqlx::query("INSERT INTO music_tag_artists(artist_id,library_id,name) VALUES ($1,$2,$3) ON CONFLICT(library_id,name_norm) DO UPDATE SET name=music_tag_artists.name RETURNING artist_id,name")
            .bind(Uuid::new_v4()).bind(library_id).bind(&name).fetch_one(&mut **tx).await?;
        let artist_id: Uuid = artist.try_get("artist_id")?;
        let canonical_name: String = artist.try_get("name")?;
        let opaque_path = format!("puffinbox://music/artists/{artist_id}");
        let inserted = sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,metadata_json) VALUES ($1,$2,NULL,$3,$4,'MusicArtist',$5,$6,$7) ON CONFLICT(id) DO UPDATE SET name=items.name WHERE items.library_id=EXCLUDED.library_id AND items.path=EXCLUDED.path AND items.item_type='MusicArtist' RETURNING id")
            .bind(artist_id).bind(library_id).bind(&canonical_name).bind(canonical_name.to_lowercase())
            .bind(&opaque_path).bind(path_hash(&opaque_path)).bind(Json(json!({})))
            .fetch_optional(&mut **tx).await?;
        if inserted.is_none() {
            return Err(sqlx::Error::Protocol(
                "music artist identity collision".to_owned(),
            ));
        }
        for (role, original) in &credits {
            if original == &source_name {
                sqlx::query("INSERT INTO music_tag_artist_sources(item_id,provider_key,role,artist_id,source_name) VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                    .bind(item_id).bind(provider).bind(*role).bind(artist_id).bind(&source_name)
                    .execute(&mut **tx).await?;
            }
        }
    }
    Ok(())
}

/// Apply this before the administrator shortcut too: inactive generated names
/// are not catalog entries. Physical folders retain their existing policy.
pub(super) fn push_visibility(builder: &mut QueryBuilder<'_, Postgres>, user: &UserRecord) {
    builder.push(" AND (NOT EXISTS(SELECT 1 FROM music_tag_artists tag WHERE tag.artist_id=i.id) OR EXISTS(SELECT 1 FROM music_tag_artist_sources credit JOIN music_tag_artists tag ON tag.artist_id=credit.artist_id JOIN items source ON source.id=credit.item_id AND source.library_id=tag.library_id JOIN item_metadata m ON m.item_id=source.id AND m.provider_key=credit.provider_key WHERE tag.artist_id=i.id AND tag.library_id=i.library_id AND i.item_type='MusicArtist' AND source.item_type='Audio' AND source.path !~ '(^|/)[.]' AND lower(btrim(credit.source_name))=tag.name_norm AND EXISTS(SELECT 1 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(m.metadata_json->credit.role)='array' THEN m.metadata_json->credit.role ELSE '[]'::jsonb END) WITH ORDINALITY n(value,ordinal) WHERE n.ordinal<=32 AND n.value=to_jsonb(credit.source_name)) AND NOT EXISTS(SELECT 1 FROM items physical WHERE physical.library_id=tag.library_id AND physical.item_type='MusicArtist' AND lower(physical.name)=tag.name_norm AND NOT EXISTS(SELECT 1 FROM music_tag_artists other WHERE other.artist_id=physical.id)) AND (m.provider_key='local-nfo' OR (source.library_id=m.source_library_id AND source.path_hash=m.source_path_hash AND source.size_bytes=m.source_size_bytes AND source.date_modified=m.source_date_modified AND NOT EXISTS(SELECT 1 FROM item_metadata preferred WHERE preferred.item_id=source.id AND preferred.provider_key='local-nfo' AND jsonb_array_length(CASE WHEN jsonb_typeof(preferred.metadata_json->credit.role)='array' THEN preferred.metadata_json->credit.role ELSE '[]'::jsonb END)>0)))");
    if !user.is_admin {
        if !user.enable_live_tv_access {
            builder
                .push(" AND COALESCE(source.metadata_json->>'LiveTvRecording','false') <> 'true'");
        }
        push_rating_visibility_filters(builder, user, "source");
    }
    builder.push(")) ");
}

#[cfg(test)]
mod tests {
    use std::{env, path::PathBuf, sync::Arc, time::Duration};

    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::connect_info::ConnectInfo,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use sqlx::{PgPool, postgres::PgPoolOptions, types::Json};
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{AppState, Config, api, auth, db, library};

    async fn embedded(pool: &PgPool, id: Uuid, names: Value) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,metadata_json,source_library_id,source_path_hash,source_size_bytes,source_date_modified) SELECT id,'embedded-audio',$2,library_id,path_hash,size_bytes,date_modified FROM items WHERE id=$1 ON CONFLICT(item_id,provider_key) DO UPDATE SET metadata_json=EXCLUDED.metadata_json,source_library_id=EXCLUDED.source_library_id,source_path_hash=EXCLUDED.source_path_hash,source_size_bytes=EXCLUDED.source_size_bytes,source_date_modified=EXCLUDED.source_date_modified")
            .bind(id).bind(Json(json!({"artists":names,"albumArtists":[]})))
            .execute(&mut *tx).await.unwrap();
        super::register_metadata_artists(&mut tx, id, "embedded-audio")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn request(
        router: &Router,
        token: &str,
        method: &str,
        path: &str,
        status: StatusCode,
    ) -> Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .extension(ConnectInfo(
                        "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actual = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(actual, status, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    async fn visible(router: &Router, token: &str, artist: Uuid, expected: bool) {
        request(
            router,
            token,
            "GET",
            &format!("/Items/{artist}"),
            if expected {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
        )
        .await;
        let listed = request(
            router,
            token,
            "GET",
            "/Items?IncludeItemTypes=MusicArtist&Recursive=true",
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            listed["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["Id"] == artist.to_string()),
            expected
        );
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
    async fn tag_artist_identities_follow_current_visible_sources_and_survive_scans() {
        let url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("puffinbox_tag_artists_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin_pool)
            .await
            .unwrap();
        let selected = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(move |connection, _| {
                let selected = selected.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO \"{selected}\""))
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let run = Uuid::new_v4();
        db::activate_run(&pool, run).await.unwrap();
        let root = env::temp_dir().join(format!("puffinbox-tag-artists-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let library = Uuid::new_v4();
        let private = Uuid::new_v4();
        for (id, name) in [(library, "Tag artists"), (private, "Private tag artists")] {
            db::insert_library(
                &pool,
                run,
                id,
                name,
                "music",
                std::slice::from_ref(&root),
                true,
            )
            .await
            .unwrap();
        }
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            public_base_url: None,
            database_url: url,
            server_name: "Tag artist test".to_owned(),
            web_root: PathBuf::from("web"),
            data_dir: root.clone(),
            ffmpeg_path: None,
            max_scan_workers: 1,
            max_page_size: 100,
            access_token_lifetime_hours: 24,
            cookie_secure: false,
            cors_origins: vec![],
            trusted_proxies: vec![],
            local_networks: vec!["127.0.0.0/8".parse().unwrap()],
            setup_token: None,
            bootstrap_admin_username: None,
            bootstrap_admin_password: None,
        };
        let state =
            AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run, None);
        let router = api::router(state.clone());
        let owner = Uuid::new_v4();
        let peer = Uuid::new_v4();
        let administrator = Uuid::new_v4();
        let mut tokens = Vec::new();
        for (id, name, is_admin, grant) in [
            (owner, "owner", false, library),
            (peer, "peer", false, private),
            (administrator, "admin", true, library),
        ] {
            sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,restrict_libraries) VALUES($1,$2,$2,'unused',$3,TRUE)")
                .bind(id).bind(name).bind(is_admin).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
                .bind(id)
                .bind(grant)
                .execute(&pool)
                .await
                .unwrap();
            let user = db::get_user(&pool, id).await.unwrap().unwrap();
            tokens.push(
                auth::issue_token(&state, &user, "tag-test", "test-client", name)
                    .await
                    .unwrap()
                    .token,
            );
        }
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        for id in [first, second] {
            let path = format!("/media/{id}.flac");
            sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES($1,$2,'04 Track','04 track','Audio',$3,$4,100,'2021-04-05T00:00:00Z')")
                .bind(id).bind(library).bind(&path).bind(db::path_hash(&path)).execute(&pool).await.unwrap();
        }
        embedded(
            &pool,
            first,
            json!([" Tag Lead ", "Literal; Guest", "bad\nname", ""]),
        )
        .await;
        embedded(&pool, second, json!(["tag lead"])).await;
        let artist: Uuid = sqlx::query_scalar(
            "SELECT artist_id FROM music_tag_artists WHERE name_norm='tag lead'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM music_tag_artists")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        let detail = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items/{first}"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(detail["Artists"], json!(["Literal; Guest", "Tag Lead"]));
        assert!(detail["IndexNumber"].is_null());
        let artist_detail = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items/{artist}"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(artist_detail["SortName"], "tag lead");
        assert!(
            detail["ArtistItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["Id"] == artist.to_string())
        );
        visible(&router, &tokens[0], artist, true).await;
        visible(&router, &tokens[1], artist, false).await;
        request(
            &router,
            &tokens[0],
            "POST",
            &format!("/UserFavoriteItems/{artist}"),
            StatusCode::OK,
        )
        .await;
        let saved: Value = sqlx::query_scalar(
            "SELECT to_jsonb(ud) FROM user_item_data ud WHERE item_id=$1 AND user_id=$2",
        )
        .bind(artist)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

        // One current accessible source is enough. Stale sources are not.
        sqlx::query("UPDATE items SET size_bytes=101 WHERE id=$1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, true).await;
        sqlx::query("UPDATE items SET size_bytes=101 WHERE id=$1")
            .bind(second)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, false).await;
        visible(&router, &tokens[2], artist, false).await;
        let stale = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items/{first}"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(stale["IndexNumber"], 4);
        embedded(&pool, first, json!(["Tag Lead"])).await;
        visible(&router, &tokens[0], artist, true).await;

        // A nonempty NFO role has priority; an empty NFO role does not.
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,metadata_json) VALUES($1,'local-nfo','{\"artists\":[\"Another artist\"]}')")
            .bind(first).execute(&pool).await.unwrap();
        visible(&router, &tokens[0], artist, false).await;
        sqlx::query("UPDATE item_metadata SET metadata_json='{\"artists\":[]}' WHERE item_id=$1 AND provider_key='local-nfo'")
            .bind(first).execute(&pool).await.unwrap();
        visible(&router, &tokens[0], artist, true).await;
        sqlx::query("UPDATE users SET block_unrated_items=ARRAY['Music']::text[] WHERE id=$1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, false).await;
        sqlx::query("UPDATE users SET block_unrated_items=ARRAY[]::text[],max_parental_rating=50 WHERE id=$1")
            .bind(owner).execute(&pool).await.unwrap();
        sqlx::query("UPDATE item_metadata SET policy_rating_scale='US-MPAA-v1',policy_rating_value=100 WHERE item_id=$1 AND provider_key='local-nfo'")
            .bind(first).execute(&pool).await.unwrap();
        visible(&router, &tokens[0], artist, false).await;
        visible(&router, &tokens[2], artist, true).await;
        sqlx::query("DELETE FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();

        // A malformed or relocated source cannot lend visibility across libraries.
        sqlx::query("UPDATE items SET library_id=$2 WHERE id=$1")
            .bind(first)
            .bind(private)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, false).await;
        visible(&router, &tokens[1], artist, false).await;
        sqlx::query("UPDATE items SET library_id=$2 WHERE id=$1")
            .bind(first)
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();
        let outside: Vec<Value> = (0..32)
            .map(|_| json!("Unregistered"))
            .chain([json!("Tag Lead")])
            .collect();
        sqlx::query("UPDATE item_metadata SET metadata_json=$2 WHERE item_id=$1 AND provider_key='embedded-audio'")
            .bind(first).bind(Json(json!({"artists":outside}))).execute(&pool).await.unwrap();
        visible(&router, &tokens[0], artist, false).await;
        embedded(&pool, first, json!(["Tag Lead"])).await;
        sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, false).await;
        visible(&router, &tokens[2], artist, false).await;
        sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();

        // Prefer a later physical identity even when its path is hidden.
        let physical = Uuid::new_v4();
        sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES($1,$2,'Tag Lead','tag lead','MusicArtist','/media/.hidden/Tag Lead',$3)")
            .bind(physical).bind(library).bind(db::path_hash("/media/.hidden/Tag Lead")).execute(&pool).await.unwrap();
        visible(&router, &tokens[0], artist, false).await;
        sqlx::query("DELETE FROM items WHERE id=$1")
            .bind(physical)
            .execute(&pool)
            .await
            .unwrap();
        visible(&router, &tokens[0], artist, true).await;
        let after: Value = sqlx::query_scalar(
            "SELECT to_jsonb(ud) FROM user_item_data ud WHERE item_id=$1 AND user_id=$2",
        )
        .bind(artist)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(saved, after);

        // Aggregate only visible current track roles onto the album. An artist
        // credited as an album artist on one track is not an album contributor.
        let album = Uuid::new_v4();
        sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES($1,$2,'Role Album','role album','MusicAlbum','/media/role-album',$3)")
            .bind(album).bind(library).bind(db::path_hash("/media/role-album")).execute(&pool).await.unwrap();
        sqlx::query("UPDATE items SET parent_id=$2 WHERE id=ANY($1)")
            .bind(vec![first, second])
            .bind(album)
            .execute(&pool)
            .await
            .unwrap();
        for (track, album_artist) in [(first, "Tag Lead"), (second, "Album Role")] {
            embedded(&pool, track, json!(["Tag Lead"])).await;
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("UPDATE item_metadata SET metadata_json=metadata_json || jsonb_build_object('albumArtists',jsonb_build_array($2::text)) WHERE item_id=$1 AND provider_key='embedded-audio'")
                .bind(track).bind(album_artist).execute(&mut *tx).await.unwrap();
            super::register_metadata_artists(&mut tx, track, "embedded-audio")
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        sqlx::query("UPDATE items SET name='Sort Plain',sort_name='sort plain' WHERE id=$1")
            .bind(second)
            .execute(&pool)
            .await
            .unwrap();
        let ids = format!("{first},{second},{album}");
        for (sort, order, expected) in [
            ("SortName", "Ascending", vec![first, second, album]),
            ("SortName", "Descending", vec![album, second, first]),
            ("Name", "Ascending", vec![first, album, second]),
        ] {
            let selected = request(
                &router,
                &tokens[0],
                "GET",
                &format!("/Items?Ids={ids}&SortBy={sort}&SortOrder={order}"),
                StatusCode::OK,
            )
            .await;
            let actual = selected["Items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
        let detail = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items/{album}"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            detail["AlbumArtists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["Name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Album Role", "Tag Lead"]
        );
        let selected=request(&router,&tokens[0],"GET",&format!("/Items?ContributingArtistIds={artist}&Recursive=true&IncludeItemTypes=Audio,MusicAlbum"),StatusCode::OK).await;
        assert_eq!(selected["TotalRecordCount"], 1);
        assert_eq!(selected["Items"][0]["Id"], second.to_string());
        let selected = request(
            &router,
            &tokens[0],
            "GET",
            &format!(
                "/Items?AlbumArtistIds={artist}&Recursive=true&IncludeItemTypes=Audio,MusicAlbum"
            ),
            StatusCode::OK,
        )
        .await;
        assert_eq!(selected["TotalRecordCount"], 2);
        assert!(
            selected["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["Id"] == album.to_string())
        );
        let children = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items?ParentId={artist}&Recursive=true"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(children["TotalRecordCount"], 0);
        for track in [first, second] {
            embedded(&pool, track, json!([])).await;
        }
        let empty = request(
            &router,
            &tokens[0],
            "GET",
            &format!("/Items/{album}"),
            StatusCode::OK,
        )
        .await;
        assert_eq!(empty["Artists"], json!([]));
        assert_eq!(empty["AlbumArtists"], json!([]));

        // Reconcile an empty real root: stale tracks go; generated identities
        // and their saved data remain. Metadata flags do not grant retention.
        let impostor = Uuid::new_v4();
        sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,metadata_json) VALUES($1,$2,'impostor','impostor','MusicArtist','puffinbox://music/artists/impostor',$3,'{\"MusicTagArtist\":true}')")
            .bind(impostor).bind(library).bind(db::path_hash("puffinbox://music/artists/impostor")).execute(&pool).await.unwrap();
        assert_eq!(
            library::spawn_scan(state.clone(), library).await.unwrap(),
            library::ScanStart::Started
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status: Option<String> =
                    sqlx::query_scalar("SELECT status FROM library_scan_state WHERE library_id=$1")
                        .bind(library)
                        .fetch_optional(&pool)
                        .await
                        .unwrap();
                if status.as_deref() == Some("completed") {
                    break;
                }
                assert_ne!(status.as_deref(), Some("failed"));
                assert_ne!(status.as_deref(), Some("completed_with_errors"));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        visible(&router, &tokens[0], artist, false).await;
        let retained: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE id=$1)")
            .bind(artist)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(retained);
        let removed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id=ANY($1)")
            .bind(vec![first, second, impostor])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(removed, 0);
        let after: Value = sqlx::query_scalar(
            "SELECT to_jsonb(ud) FROM user_item_data ud WHERE item_id=$1 AND user_id=$2",
        )
        .bind(artist)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(saved, after);
        state
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::Release);
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
            .execute(&admin_pool)
            .await
            .unwrap();
        admin_pool.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
