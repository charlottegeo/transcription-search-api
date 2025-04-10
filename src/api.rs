use crate::db::{setup_database, remove_cache};
use crate::file_parser;
use crate::models::{Episode, Line, RandomLineQuery, SearchPhrasesQuery, Season, Speaker, UserQuery};
use actix_multipart::Multipart;
use actix_web::{get, post, web, HttpResponse, Responder};
use futures_util::stream::TryStreamExt;
use regex::Regex;
use sanitize_filename::sanitize;
use serde_json::{json, Value};
use sqlx::{SqlitePool, Row};
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use zip::ZipArchive;
pub type DatabaseRegistry = Arc<Mutex<HashMap<String, SqlitePool>>>;


///Regex for FTS
fn escape_fts5_query(query: &str) -> String {
    let re = Regex::new(r"[^\\w\\s]").unwrap();
    re.replace_all(query, " ").to_string()
}

///Gets a database connection pool based on a UID
async fn get_db_pool(
    db_registry: &DatabaseRegistry,
    user_id: Option<&str>,
) -> Result<SqlitePool, HttpResponse> {
    let id = user_id.unwrap_or("default");
    let registry = db_registry.lock().await;
    registry.get(id).cloned().ok_or_else(|| {
        HttpResponse::NotFound().json(json!({"error": format!("Database not found for user: {}", id)}))
    })
}

///Removes a user's database from the registry and deletes the db file
async fn cleanup(
    db_registry: web::Data<DatabaseRegistry>,
    user_id: &str,
) -> HttpResponse {
    let removed = {
        let mut registry = db_registry.lock().await;
        registry.remove(user_id)
    };

    let db_path = format!("./temp_dbs/{}.sqlite", user_id);
    if let Err(err) = tokio::fs::remove_file(&db_path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!("Failed to remove database file {}: {}", db_path, err);
            return HttpResponse::InternalServerError().body("Failed to clean up database");
        }
    }

    if removed.is_some() {
        HttpResponse::Ok().body("Database cleaned up successfully")
    } else {
        HttpResponse::NotFound().body("User database not found")
    }
}

///Ensures zip file is "valid" or not empty
fn verify_zip_file(path: &str) -> io::Result<()> {
    let file = std::fs::File::open(path)?;
    let archive = ZipArchive::new(file)?;
    if archive.len() == 0 {
        Err(io::Error::new(io::ErrorKind::InvalidData, "ZIP file is empty"))
    } else {
        Ok(())
    }
}

///Opens the given zip file, and extracts all text files to the output directory
async fn extract_zip(zip_path: &str, output_dir: &str) -> io::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;

    fs::create_dir_all(output_dir).await?;
    for i in 0..archive.len() {
        let mut zip_file = archive.by_index(i)?;
        let outpath = Path::new(output_dir).join(zip_file.name());
        if let Some(parent) = outpath.parent() {
            fs::create_dir_all(parent).await?;
        }
        if !zip_file.name().ends_with(".txt") {
            continue;
        }
        let mut outfile = fs::File::create(&outpath).await?;
        let mut buffer = vec![0; 8192];
        loop {
            let bytes_read = zip_file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            outfile.write_all(&buffer[..bytes_read]).await?;
        }
    }
    Ok(())
}

///Gets the 2 lines before and after the given line (this is useful for frontend to see the context of the search result)
async fn get_context_lines(db_pool: &SqlitePool, line: &Line) -> Vec<Line> {
    let context_query = r#"
        SELECT 
            lines.id,
            lines.season_id,
            lines.episode_id,
            lines.speaker_id,
            speakers.name AS speaker_name,
            lines.line_number,
            lines.content
        FROM lines
        LEFT JOIN speakers ON lines.speaker_id = speakers.id
        WHERE lines.episode_id = ?
        AND lines.line_number BETWEEN ? AND ?
        ORDER BY lines.line_number ASC
    "#;

    sqlx::query_as(context_query)
        .bind(line.episode_id)
        .bind(line.line_number.saturating_sub(2).max(1))
        .bind(line.line_number.saturating_add(2))
        .fetch_all(db_pool)
        .await
        .unwrap_or_else(|err| {
            eprintln!("Context query failed: {}", err);
            Vec::new()
        })
}

///Endpoint to clean up a user's database
#[post("/cleanup")]
async fn cleanup_db(
    db_registry: web::Data<DatabaseRegistry>,
    body: web::Json<Value>,
) -> impl Responder {
    if let Some(user_id) = body.get("user_id").and_then(|v| v.as_str()) {
        cleanup(db_registry, user_id).await
    } else {
        HttpResponse::BadRequest().body("Missing user_id")
    }
}

///Endpoint to handle file uploads
#[post("/upload")]
async fn upload_zip(
    mut payload: Multipart,
    db_registry: web::Data<DatabaseRegistry>,
    schema_path: web::Data<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let user_id = "default".to_string();

    //Makes a temporary directory to extract files to
    let temp_dir = "./temp_uploads";
    let extract_dir_path = format!("{}/{}", temp_dir, user_id);
    let extract_dir = Path::new(&extract_dir_path);

    fs::create_dir_all(&extract_dir).await.map_err(|_| {
        actix_web::error::ErrorInternalServerError("Failed to create directory")
    })?;
    {
        let mut registry = db_registry.lock().await;
        registry.remove(&user_id);
        let _ = fs::remove_file(format!("./temp_dbs/{}.sqlite", user_id)).await;
    }
    remove_cache(&user_id).await;
    let mut saved_file_path = String::new();

    while let Some(mut field) = payload.try_next().await.map_err(|_| {
        actix_web::error::ErrorInternalServerError("Failed to process upload")
    })? {
        if field.content_disposition().and_then(|cd| cd.get_name()) == Some("file") {
            let filename = field
                .content_disposition()
                .and_then(|cd| cd.get_filename())
                .ok_or_else(|| actix_web::error::ErrorBadRequest("No filename provided"))?;
            saved_file_path = format!("{}/{}", temp_dir, sanitize(filename));
            let mut f = fs::File::create(&saved_file_path).await?;
            while let Some(chunk) = field.try_next().await? {
                f.write_all(&chunk).await?;
            }
            f.flush().await?;
            drop(f);
        }
    }
    if saved_file_path.is_empty() {
        return Ok(HttpResponse::BadRequest().json(json!({"error": "No file uploaded"})));
    }
    if let Err(err) = verify_zip_file(&saved_file_path) {
        return Ok(HttpResponse::BadRequest().json(json!({"error": format!("Invalid ZIP file: {}", err)})));
    }
    if let Err(err) = extract_zip(&saved_file_path, extract_dir.to_str().unwrap_or(""))
        .await
    {
        return Ok(HttpResponse::InternalServerError().json(json!({"error": err.to_string()})));
    }

    //Sets up database connection
    let db_pool = match get_db_pool(&db_registry, Some("default")).await {
        Ok(pool) => pool,
        Err(_) => {
            let (pool, _) = setup_database("default", schema_path.get_ref()).await.map_err(|_| {
                actix_web::error::ErrorInternalServerError("Failed to setup database")
            })?;
            db_registry.lock().await.insert("default".to_string(), pool.clone());
            pool
        }
    };    

    //processes the text files
    let result = file_parser::process_seasons(&db_pool, extract_dir, &user_id).await;
    fs::remove_file(&saved_file_path).await.ok();
    fs::remove_dir_all(&extract_dir).await.ok();

    match result {
        Ok(_) => Ok(HttpResponse::Ok().json(json!({"message": "Upload and processing successful"}))),
        Err(e) => Ok(HttpResponse::InternalServerError().json(json!({"error": e.to_string()}))),
    }
}

///Endpoint to search for phrases in the database with optional filtering for seasons, episodes, or speakers
#[get("/search/phrases")]
async fn search_phrases(
    db_registry: web::Data<DatabaseRegistry>,
    query: web::Query<SearchPhrasesQuery>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };

    let phrase = query.phrase.clone().unwrap_or_default();
    let phrase_query = if query.similar_search.unwrap_or(false) {
        format!("\"{}\"", escape_fts5_query(&phrase))
    } else {
        format!("MATCH {}", escape_fts5_query(&phrase))
    };

    let mut conditions = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if !phrase_query.is_empty() {
        conditions.push("fts.content MATCH ?");
        params.push(phrase_query);
    }
    if let Some(season) = query.season {
        conditions.push("sn.number = ?");
        params.push(season.to_string());
    }
    if let Some(episode) = query.episode {
        conditions.push("e.id = ?");
        params.push(episode.to_string());
    }
    if let Some(speaker) = query.speaker {
        conditions.push("l.speaker_id = ?");
        params.push(speaker.to_string());
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql_query = format!(
        r#"
        SELECT 
            l.id, 
            l.season_id, 
            l.episode_id, 
            l.speaker_id, 
            s.name AS speaker_name, 
            l.line_number,  
            l.content
        FROM lines l
        JOIN lines_fts fts ON l.id = fts.rowid
        LEFT JOIN speakers s ON l.speaker_id = s.id
        JOIN episodes e ON l.episode_id = e.id
        JOIN seasons sn ON e.season_id = sn.id
        {}
        ORDER BY 
            sn.number ASC,
            e.number ASC,
            l.line_number ASC
        "#,
        where_clause
    );

    let mut query_builder = sqlx::query_as::<_, Line>(&sql_query);
    for param in params {
        query_builder = query_builder.bind(param);
    }
    let results = match query_builder.fetch_all(&db_pool).await {
        Ok(data) => data,
        Err(err) => {
            eprintln!("Error executing search: {}", err);
            return HttpResponse::InternalServerError().body("Error executing search");
        }
    };
    if results.is_empty() {
        eprintln!("No results found for phrase search.");
        return HttpResponse::NotFound().json(json!({"error": "No matching results"}));
    }
    let mut response_data = Vec::new();
    for line in results.iter() {
        let context_lines = get_context_lines(&db_pool, line).await;
        response_data.push((line.clone(), context_lines));
    }
    HttpResponse::Ok().json(response_data)
}

///Endpoint to get a random line from the database with options to filter by season, episode, and/or speaker using their IDs
#[get("/random-line")]
async fn get_random_line(
    db_registry: web::Data<DatabaseRegistry>,
    query: web::Query<RandomLineQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, None).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };
    let mut sql = String::from(
        r#"
        SELECT 
            l.id, 
            l.season_id,
            l.episode_id,
            l.speaker_id,
            s.name AS speaker_name,
            l.line_number,
            l.content
        FROM lines l
        LEFT JOIN speakers s ON l.speaker_id = s.id
        JOIN episodes e ON l.episode_id = e.id
        JOIN seasons sn ON e.season_id = sn.id
        "#,
    );

    let mut conditions = Vec::new();
    let mut binds = Vec::new();
    if let Some(season) = query.season {
        conditions.push("sn.number = ?");
        binds.push(season);
    }
    if let Some(episode) = query.episode {
        conditions.push("e.id = ?");
        binds.push(episode);
    }
    if let Some(speaker) = query.speaker {
        conditions.push("l.speaker_id = ?");
        binds.push(speaker);
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY RANDOM() LIMIT 1");
    let mut query_builder = sqlx::query_as::<_, Line>(&sql);
    for value in binds {
        query_builder = query_builder.bind(value);
    }
    match query_builder.fetch_optional(&db_pool).await {
        Ok(Some(line)) => HttpResponse::Ok().json(line),
        Ok(None) => {
            eprintln!("No random line found matching the filters.");
            HttpResponse::NotFound().json(json!({"error": "No matching line found"}))
        },
        Err(e) => {
            eprintln!("Error fetching random line: {:?}", e);
            HttpResponse::InternalServerError().body("Error fetching random line")
        }
    }
}

///Endpoint to get an episode's transcript consisting of speakers + their lines, using the season + episode numbers
#[get("/transcripts/{season_num}/{episode_num}")]
async fn get_transcript(
    db_registry: web::Data<DatabaseRegistry>,
    path: web::Path<(i64, i32)>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };
    
    let (season_num, episode_num) = path.into_inner();
    let season_exists: i64 = match sqlx::query("SELECT EXISTS(SELECT 1 FROM seasons WHERE number = ?)")
        .bind(season_num)
        .fetch_one(&db_pool)
        .await
    {
        Ok(row) => row.get::<i64, _>(0),
        Err(err) => {
            eprintln!("Error checking season: {}", err);
            return HttpResponse::InternalServerError().body("Error getting season, does not exist.");
        }
    };
    if season_exists == 0 {
        return HttpResponse::NotFound().body(format!("Season {} not found", season_num));
    }
    let episode_exists: i64 = match sqlx::query("SELECT EXISTS(SELECT 1 FROM episodes WHERE number = ?)")
        .bind(episode_num)
        .fetch_one(&db_pool)
        .await
    {
        Ok(row) => row.get::<i64, _>(0),
        Err(err) => {
            eprintln!("Error checking episode existence: {}", err);
            return HttpResponse::InternalServerError().body("Error getting episode, does not exist");
        }
    };
    if episode_exists == 0 {
        return HttpResponse::NotFound().body(format!("Episode {} not found", episode_num));
    }
    let query = r#"
    SELECT 
        l.id, 
        l.season_id, 
        l.episode_id, 
        l.speaker_id, 
        s.name AS speaker_name, 
        l.line_number, 
        l.content
    FROM lines l
    LEFT JOIN speakers s ON l.speaker_id = s.id
    JOIN episodes e ON l.episode_id = e.id
    JOIN seasons sn ON e.season_id = sn.id
    WHERE sn.number = ? AND e.number = ?
    "#;

    let transcript = sqlx::query_as::<_, Line>(query)
        .bind(season_num)
        .bind(episode_num)
        .fetch_all(&db_pool)
        .await;

    match transcript {
        Ok(lines) => HttpResponse::Ok().json(lines),
        Err(err) => {
            eprintln!("Error fetching transcript: {}", err);
            HttpResponse::InternalServerError().body("Error fetching transcript")
        }
    }
}

///Endpoint to get a list of seasons
#[get("/seasons")]
async fn get_seasons(
    db_registry: web::Data<DatabaseRegistry>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };
    let seasons = match sqlx::query_as::<_, Season>("SELECT * FROM seasons")
        .fetch_all(&db_pool)
        .await
    {
        Ok(data) => data,
        Err(err) => {
            eprintln!("Error fetching seasons: {}", err);
            return HttpResponse::InternalServerError().body("Error fetching seasons");
        }
    };
    HttpResponse::Ok().json(seasons)
}

///Endpoint to get a list of speakers
#[get("/speakers")]
async fn get_speakers(
    db_registry: web::Data<DatabaseRegistry>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };
    let speakers = match sqlx::query_as::<_, Speaker>("SELECT * FROM speakers")
        .fetch_all(&db_pool)
        .await
    {
        Ok(data) => data,
        Err(err) => {
            eprintln!("Error fetching speakers: {}", err);
            return HttpResponse::InternalServerError().body("Error fetching speakers");
        }
    };
    HttpResponse::Ok().json(speakers)
}

///Endpoint to get all episodes in a season by the season's ID
#[get("/seasons/{season_id}/episodes")]
async fn get_episodes(
    db_registry: web::Data<DatabaseRegistry>,
    season_id: web::Path<i64>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };
    let episodes = match sqlx::query_as::<_, Episode>(
        "SELECT * FROM episodes WHERE season_id = ? ORDER BY number ASC",
    )
    .bind(season_id.into_inner())
    .fetch_all(&db_pool)
    .await
    {
        Ok(data) => data,
        Err(err) => {
            eprintln!("Error fetching episodes: {}", err);
            return HttpResponse::InternalServerError().body("Error fetching episodes");
        }
    };
    HttpResponse::Ok().json(episodes)
}

///Endpoint to get an episode by its ID
#[get("/episodes/{episode_id}")]
async fn get_episode(
    db_registry: web::Data<DatabaseRegistry>,
    episode_id: web::Path<i64>,
    user_query: web::Query<UserQuery>,
) -> impl Responder {
    let db_pool = match get_db_pool(&db_registry, Some(&user_query.user_id)).await {
        Ok(pool) => pool,
        Err(resp) => return resp,
    };

    match sqlx::query_as::<_, Episode>(
        "SELECT * FROM episodes WHERE id = ? LIMIT 1"
    )
    .bind(episode_id.into_inner())
    .fetch_optional(&db_pool)
    .await
    {
        Ok(Some(data)) => HttpResponse::Ok().json(data),
        Ok(None) => HttpResponse::NotFound().body("Episode not found"),
        Err(err) => {
            eprintln!("Error fetching episode: {}", err);
            HttpResponse::InternalServerError().body("Error fetching episode")
        }
    }
}

///Defines /api scope and registers endpoints
pub fn init_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api")
            .service(cleanup_db)
            .service(search_phrases)
            .service(get_random_line)
            .service(get_transcript)
            .service(get_seasons)
            .service(get_speakers)
            .service(get_episodes)
            .service(get_episode)
            .service(upload_zip)
    );
}

