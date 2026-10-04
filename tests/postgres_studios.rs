use axum::{
    Router,
    body::{Body, to_bytes},
    extract::connect_info::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Json};
use std::{env, path::PathBuf, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn studio_browse_counts_selectors_and_favorites_follow_current_visibility() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_studios_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let selected_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |connection, _| {
            let schema = selected_schema.clone();
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
    let run = Uuid::new_v4();
    db::activate_run(&pool, run).await.unwrap();
    let owner = Uuid::new_v4();
    let peer = Uuid::new_v4();
    let administrator = Uuid::new_v4();
    for (user, name, admin) in [
        (owner, "studio-owner", false),
        (peer, "studio-peer", false),
        (administrator, "studio-admin", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating) VALUES ($1,$2,$2,'synthetic-unused',$3,TRUE,TRUE,50)")
            .bind(user).bind(name).bind(admin).execute(&pool).await.unwrap();
    }
    let library = Uuid::new_v4();
    let private = Uuid::new_v4();
    let disabled = Uuid::new_v4();
    for (id, name) in [
        (library, "Visible studios"),
        (private, "Private studios"),
        (disabled, "Disabled studios"),
    ] {
        db::insert_library(
            &pool,
            run,
            id,
            name,
            "movies",
            &[PathBuf::from("/media")],
            true,
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    for (user, lib) in [(owner, library), (peer, private)] {
        sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
            .bind(user)
            .bind(lib)
            .execute(&pool)
            .await
            .unwrap();
    }
    let folder = item(
        &pool,
        library,
        None,
        "Folder",
        "/media/movies",
        json!([]),
        None,
    )
    .await;
    let first = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/movies/first.mkv",
        json!(["Alpha", "Shared", "Alpha"]),
        None,
    )
    .await;
    let second = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/movies/second.mkv",
        json!(["Shared", "Zulu"]),
        None,
    )
    .await;
    for (lib, path, rating) in [
        (private, "/media/private.mkv", None),
        (disabled, "/media/disabled.mkv", None),
        (library, "/media/.hidden.mkv", None),
        (library, "/media/adult.mkv", Some(90)),
    ] {
        item(
            &pool,
            lib,
            None,
            "Movie",
            path,
            json!(["Secret", "Shared"]),
            rating,
        )
        .await;
    }
    let album = item(
        &pool,
        library,
        None,
        "MusicAlbum",
        "/media/album",
        json!(["Record Studio"]),
        None,
    )
    .await;
    item(
        &pool,
        library,
        Some(album),
        "Audio",
        "/media/album/song.flac",
        json!([]),
        None,
    )
    .await;
    // Remote provider output must not invent studio credits beside the NFO.
    sqlx::query(
        "INSERT INTO item_metadata(item_id,provider_key,metadata_json) VALUES ($1,'tvmaze',$2)",
    )
    .bind(first)
    .bind(Json(json!({"studios":["Untrusted"]})))
    .execute(&pool)
    .await
    .unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Studio fixture".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(&schema),
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
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run, None);
    let mut tokens = Vec::new();
    for user in [owner, peer, administrator] {
        tokens.push(
            auth::issue_token(
                &state,
                &db::get_user(&pool, user).await.unwrap().unwrap(),
                "studio-fixture",
                "fixture",
                "studio-client",
            )
            .await
            .unwrap()
            .token,
        );
    }
    let router = api::router(state);
    let token = &tokens[0];
    assert_eq!(
        call(&router, "GET", "/Studios", None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(&router, "GET", "/Studios", Some(token)).await;
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let all = body(response).await;
    assert_eq!(
        names(&all),
        vec!["Alpha", "Record Studio", "Shared", "Zulu"]
    );
    assert_eq!(all["TotalRecordCount"], 4);
    let shared = all["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Name"] == "Shared")
        .unwrap();
    assert_eq!(shared["ChildCount"], 2);
    assert_eq!(shared["MovieCount"], 2);
    assert_eq!(shared["SongCount"], 0);
    let id = shared["Id"].as_str().unwrap();
    let detail = body(call(&router, "GET", "/Studios/Shared", Some(token)).await).await;
    assert_eq!(detail["Id"], id);
    assert_eq!(detail["UserData"]["IsFavorite"], false);
    assert_eq!(
        body(call(&router, "GET", &format!("/Items/{id}"), Some(token)).await).await["Type"],
        "Studio"
    );
    let item_dto = body(call(&router, "GET", &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(item_dto["Studios"].as_array().unwrap().len(), 2);
    assert_eq!(item_dto["Studios"][1]["Id"], id);
    let selected = body(
        call(
            &router,
            "GET",
            &format!("/Items?Recursive=true&StudioIds={id}&Limit=1&StartIndex=1"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(selected["TotalRecordCount"], 2);
    assert_eq!(selected["Items"][0]["Id"], second.to_string());
    // The official studio movie view requests this multi-field sort. Exercise
    // both groups and field directions so accepting the name alone cannot pass.
    for (order, expected) in [
        ("Ascending", vec![first, second, folder]),
        ("Descending", vec![folder, second, first]),
        ("Descending,Ascending", vec![folder, first, second]),
    ] {
        let response = call(
            &router,
            "GET",
            &format!(
                "/Items?Recursive=true&IncludeItemTypes=Folder,Movie&SortBy=IsFolder,SortName&SortOrder={order}"
            ),
            Some(token),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let sorted = body(response).await;
        assert_eq!(sorted["TotalRecordCount"], 3);
        assert_eq!(
            sorted["Items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["Id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected.iter().map(Uuid::to_string).collect::<Vec<_>>()
        );
    }
    let response = call(
        &router,
        "GET",
        "/Items?Recursive=true&IncludeItemTypes=Folder,Movie&SortBy=isfolder,sortname&StartIndex=1&Limit=1",
        Some(token),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let sorted_page = body(response).await;
    assert_eq!(sorted_page["TotalRecordCount"], 3);
    assert_eq!(sorted_page["Items"][0]["Id"], second.to_string());
    let response = call(
        &router,
        "GET",
        &format!(
            "/Users/{owner}/Items?Recursive=true&StudioIds={id}&SortBy=IsFolder,SortName&SortOrder=Ascending&Fields=PrimaryImageAspectRatio,SortName,PrimaryImageAspectRatio&ImageTypeLimit=1"
        ),
        Some(token),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["TotalRecordCount"], 2);
    for (query, expected) in [
        ("SearchTerm=HAR", vec!["Shared"]),
        ("NameStartsWith=s", vec!["Shared"]),
        ("NameStartsWithOrGreater=Shared", vec!["Shared", "Zulu"]),
        ("NameLessThan=Record", vec!["Alpha"]),
        ("IncludeItemTypes=Movie", vec!["Alpha", "Shared", "Zulu"]),
        ("ExcludeItemTypes=Movie", vec!["Record Studio"]),
    ] {
        assert_eq!(
            names(
                &body(call(&router, "GET", &format!("/Studios?{query}"), Some(token)).await).await
            ),
            expected
        );
    }
    let localized = body(
        call(
            &router,
            "GET",
            &format!("/Studios?ParentId={folder}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(names(&localized), vec!["Alpha", "Shared", "Zulu"]);
    let page = body(call(&router, "GET", "/Studios?StartIndex=1&Limit=1", Some(token)).await).await;
    assert_eq!(names(&page), vec!["Record Studio"]);
    assert_eq!(page["TotalRecordCount"], 4);
    let zero = body(call(&router, "GET", "/Studios?Limit=0", Some(token)).await).await;
    assert_eq!(names(&zero), Vec::<&str>::new());
    assert_eq!(zero["TotalRecordCount"], 4);
    let options = body(
        call(
            &router,
            "GET",
            "/Studios?EnableTotalRecordCount=false&EnableUserData=false&EnableImages=false",
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(options["TotalRecordCount"], 0);
    assert!(options["Items"][0].get("UserData").is_none());
    assert!(options["Items"][0].get("ImageTags").is_none());
    let favorite = format!("/UserFavoriteItems/{id}");
    assert_eq!(
        body(call(&router, "POST", &favorite, Some(token)).await).await["IsFavorite"],
        true
    );
    assert_eq!(
        names(&body(call(&router, "GET", "/Studios?IsFavorite=true", Some(token)).await).await),
        vec!["Shared"]
    );
    assert_eq!(
        body(
            call(
                &router,
                "GET",
                &format!("/Items/{id}/UserData"),
                Some(token)
            )
            .await
        )
        .await["IsFavorite"],
        true
    );
    assert_eq!(
        names(&body(call(&router, "GET", "/Studios?IsFavorite=false", Some(token)).await).await),
        vec!["Alpha", "Record Studio", "Zulu"]
    );
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/Studios?UserId={peer}"),
            Some(token)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        names(
            &body(
                call(
                    &router,
                    "GET",
                    &format!("/Studios?UserId={owner}"),
                    Some(&tokens[2])
                )
                .await
            )
            .await
        ),
        names(&all)
    );
    assert_eq!(
        call(&router, "GET", "/Studios/Secret", Some(token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/Studios?ParentId={private}"),
            Some(token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&router, "POST", &favorite, Some(&tokens[1]))
            .await
            .status(),
        StatusCode::OK
    ); // Shared is visible in peer's own library.
    assert_eq!(
        body(call(&router, "DELETE", &favorite, Some(token)).await).await["IsFavorite"],
        false
    );
    assert_eq!(
        names(&body(call(&router, "GET", "/Studios?IsFavorite=true", Some(token)).await).await),
        Vec::<&str>::new()
    );
    // Favorites never grant access after a policy change, even through item ID.
    body(call(&router, "POST", &favorite, Some(token)).await).await;
    sqlx::query("DELETE FROM user_library_access WHERE user_id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        names(&body(call(&router, "GET", "/Studios?IsFavorite=true", Some(token)).await).await),
        Vec::<&str>::new()
    );
    for path in [
        format!("/Items/{id}"),
        format!("/Items/{id}/UserData"),
        "/Studios/Shared".to_owned(),
    ] {
        assert_eq!(
            call(&router, "GET", &path, Some(token)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        call(&router, "DELETE", &favorite, Some(token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    for path in [
        "/Studios?Limit=-1",
        "/Studios?Limit=101",
        "/Studios?StartIndex=-1",
        "/Studios?NameStartsWith=%00",
        "/Items?StudioIds=invalid",
        "/Studios?IncludeItemTypes=invalid",
    ] {
        assert_eq!(
            call(&router, "GET", path, Some(&tokens[2])).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

async fn item(
    pool: &PgPool,
    library: Uuid,
    parent: Option<Uuid>,
    kind: &str,
    path: &str,
    studios: Value,
    rating: Option<i16>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,$3,$4,$4,$5,$4,$6)").bind(id).bind(library).bind(parent).bind(path).bind(kind).bind(db::path_hash(path)).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,metadata_json,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo',$2,CASE WHEN $3::smallint IS NOT NULL THEN 'US-MPAA-v1' END,$3)").bind(id).bind(Json(json!({"studios":studios}))).bind(rating).execute(pool).await.unwrap();
    id
}

async fn call(router: &Router, method: &str, path: &str, token: Option<&str>) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .extension(ConnectInfo(
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

async fn body(response: Response) -> Value {
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

fn names(page: &Value) -> Vec<&str> {
    page["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Name"].as_str().unwrap())
        .collect()
}
