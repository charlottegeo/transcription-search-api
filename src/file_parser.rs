use std::path::Path;
use tokio::{
    fs::File,
    io::{self as tokio_io, AsyncBufReadExt, AsyncWriteExt, BufReader},
};
use regex::Regex;
use walkdir::WalkDir;
use sqlx::SqlitePool;
use std::collections::HashMap;

///Parses a given filename to get the season + episode numbers, and episode title using regex
fn parse_episode_filename(filename: &str, parent_dir: Option<&str>) -> Option<(i32, i32, String)> {
    let filename = filename.trim();
    let season_episode_regex = Regex::new(r"(?i)^(\d{1,2})x(\d{1,2})\s*-\s*(.+)\.txt$").ok()?;
    let sxxe_regex = Regex::new(r"(?i)^s(\d{1,2})e(\d{1,2})(?:\s*-\s*(.+))?\.txt$").ok()?;
    let e_regex = Regex::new(r"(?i)^e(\d+)\s*-\s*(.+)\.txt$").ok()?;    
    let season_dir_regex = Regex::new(r"(?i)(?:season)?\s*S?(\d{1,2})").ok()?;

    if let Some(caps) = season_episode_regex.captures(filename) {
        let season_num = caps[1].parse().ok()?;
        let episode_num = caps[2].parse().ok()?;
        let title = caps[3].trim().to_string();
        Some((season_num, episode_num, title))
    } else if let Some(caps) = sxxe_regex.captures(filename) {
        let season_num = caps[1].parse().ok()?;
        let episode_num = caps[2].parse().ok()?;
        let title = caps.get(3).map_or("", |m| m.as_str()).trim().to_string();
        Some((season_num, episode_num, title))
    } else if let Some(caps) = e_regex.captures(filename) {
        let season_num = parent_dir.and_then(|parent| {
            season_dir_regex
                .captures(parent)?
                .get(1)?
                .as_str()
                .parse::<i32>()
                .ok()
        })?;
        let episode_num = caps[1].parse().ok()?;
        let title = caps[2].trim().to_string();
        Some((season_num, episode_num, title))
    } else {
        None
    }
}

///Iterates through the directory and gets all text files, sorts them, uses regex to get episode data, then inserts the speakers + lines into the database
pub async fn process_seasons(
    pool: &SqlitePool,
    extract_dir: &Path,
    _user_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !extract_dir.exists() {
        return Err(Box::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Directory not found: {}", extract_dir.display()),
        )));
    }

    let mut transaction = pool.begin().await?;
    let mut entries: Vec<_> = WalkDir::new(extract_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().map_or(false, |ext| ext == "txt"))
        .collect();

    if entries.is_empty() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No valid text files found",
        )));
    }

    entries.sort_by_key(|e| e.path().file_name().map(|n| n.to_os_string()));
    let mut season_episodes: HashMap<i32, Vec<_>> = HashMap::new();
    for entry in &entries {
        let filename = entry.file_name().to_string_lossy();
        let parent_dir = entry.path().parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy());

        if let Some((season_num, episode_num, title)) =
            parse_episode_filename(&filename, parent_dir.as_deref())
        {
            season_episodes.entry(season_num).or_default().push((episode_num, title, entry));
        }
    }

    let total_episodes: usize = season_episodes.values().map(|v| v.len()).sum();
    let mut episodes_processed = 0;
    let mut sorted_seasons: Vec<_> = season_episodes.into_iter().collect();
    sorted_seasons.sort_by_key(|(season_num, _)| *season_num);

    for (season_num, mut episodes) in sorted_seasons {

        //Adds season into database
        let season_id: i64 = sqlx::query_scalar(
            "INSERT INTO seasons (number) VALUES (?) ON CONFLICT(number) DO UPDATE SET number = excluded.number RETURNING id",
        )
        .bind(season_num)
        .fetch_one(&mut *transaction)
        .await?;

        episodes.sort_by_key(|(num, title, _)| (*num, title.clone()));
        for (episode_num, title, entry) in episodes {
            episodes_processed += 1;

            //should keep track of parsing progress in terminal
            let progress = format!(
                "\rProcessing episode {}/{}: S{:02}E{:02} {:<40}",
                episodes_processed,
                total_episodes,
                season_num,
                episode_num,
                title
            );
            let _ = tokio_io::stdout().write_all(progress.as_bytes()).await;
            let _ = tokio_io::stdout().flush().await;

            //Adds episode associated with season into database
            let episode_id: i64 = sqlx::query_scalar(
                "INSERT INTO episodes (season_id, number, title) VALUES (?, ?, ?) ON CONFLICT(season_id, number) DO UPDATE SET title = excluded.title RETURNING id",
            )
            .bind(season_id)
            .bind(episode_num)
            .bind(&title)
            .fetch_one(&mut *transaction)
            .await?;

            let file = File::open(entry.path()).await?;
            let mut reader = BufReader::new(file).lines();
            let mut line_num = 1;

            //Iterates through each line in the text file and inserts the line and speaker into the database
            while let Some(line_result) = reader.next_line().await? {
                let line = line_result;
                let (speaker_id, content) = if let Some((speaker, content)) = line.split_once(':') {
                    let speaker_id: i64 = sqlx::query_scalar(
                        "INSERT INTO speakers (name) VALUES (?) ON CONFLICT(name) DO UPDATE SET name = excluded.name RETURNING id",
                    )
                    .bind(speaker.trim())
                    .fetch_one(&mut *transaction)
                    .await?;
                    (Some(speaker_id), content.trim().to_string())
                } else {
                    (None, line.trim().to_string())
                };

                sqlx::query("INSERT INTO lines (season_id, episode_id, speaker_id, line_number, content) VALUES (?, ?, ?, ?, ?)")
                    .bind(season_id)
                    .bind(episode_id)
                    .bind(speaker_id)
                    .bind(line_num)
                    .bind(&content)
                    .execute(&mut *transaction)
                    .await?;

                line_num += 1;
            }
        }
    }

    let _ = tokio_io::stdout().write_all(b"\nDone parsing.\n").await;
    let _ = tokio_io::stdout().flush().await;
    transaction.commit().await?;
    tokio::fs::remove_dir_all(extract_dir).await?;
    Ok(())
}
