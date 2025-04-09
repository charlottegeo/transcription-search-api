use lazy_static::lazy_static;
use sqlx::migrate::MigrateDatabase;
use sqlx::{Sqlite, SqlitePool};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::Mutex;
use std::collections::HashMap;

lazy_static! {
    static ref DB_CACHE: Mutex<HashMap<String, SqlitePool>> = Mutex::new(HashMap::new());
}

///Removes a user's database connection from cache
pub async fn remove_cache(user_id: &str) {
    let mut cache = DB_CACHE.lock().await;
    cache.remove(user_id);
}

///Makes a database for a user and applies schema, and returns the connection pool and database file path.
pub async fn setup_database(
    user_id: &str,
    schema_path: &str,
) -> Result<(SqlitePool, PathBuf), Box<dyn std::error::Error>> {
    let db_path = Path::new("./temp_dbs").join(format!("{}.sqlite", user_id));
    println!("Setting up database at {:?}", db_path);

    fs::create_dir_all("./temp_dbs").await?;
    let database_url = format!("sqlite:{}", db_path.to_string_lossy());
    let db_exists = Sqlite::database_exists(&database_url)
        .await
        .unwrap_or(false);
    let mut cache = DB_CACHE.lock().await;
    if let Some(pool) = cache.get(user_id) {
        return Ok((pool.clone(), db_path));
    }

    let db_pool = if !db_exists {
        if let Err(err) = Sqlite::create_database(&database_url).await {
            eprintln!("Failed to create database: {}", err);
            return Err(Box::new(err));
        }

        println!("Database created: {}", database_url);
        let pool = SqlitePool::connect(&database_url).await?;
        println!("Connected to database.");

        match tokio::fs::read_to_string(schema_path).await {
            Ok(schema) => {
                if let Err(err) = sqlx::query(&schema).execute(&pool).await {
                    eprintln!("Schema execution failed: {}", err);
                    return Err(Box::new(err));
                }
                println!("Schema applied");
            }
            Err(err) => {
                eprintln!("Failed to read schema: {}", err);
                return Err(Box::new(err));
            }
        }

        pool
    } else {
        println!("Connecting to existing database: {}", database_url);
        SqlitePool::connect(&database_url).await?
    };

    cache.insert(user_id.to_string(), db_pool.clone());
    Ok((db_pool, db_path))
}
