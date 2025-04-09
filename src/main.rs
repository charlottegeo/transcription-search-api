mod api;
mod db;
mod file_parser;
mod models;
use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use api::init_routes;
use dotenv::dotenv;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::fs;

pub type DatabaseRegistry = Arc<Mutex<HashMap<String, SqlitePool>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    let schema_path = "schema.sql".to_string();
    let db_registry: DatabaseRegistry = Arc::new(Mutex::new(HashMap::new()));
    if Path::new("./temp_dbs").exists() {
        if let Err(err) = fs::remove_dir_all("./temp_dbs") {
            eprintln!("Failed to clean up temp_dbs directory: {}", err);
        }
    }

    //creates database to hold dbs for users, this is mainly useful for when I eventually complete the frontend + make a site
    fs::create_dir_all("./temp_dbs")?;
    println!("Backend running");

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header();
        App::new()
            .wrap(cors)
            .app_data(web::Data::new(db_registry.clone()))
            .app_data(web::Data::new(schema_path.clone()))
            .configure(init_routes)
    })

    .bind("0.0.0.0:8081")?
    .run()
    .await?;

    Ok(())
}
