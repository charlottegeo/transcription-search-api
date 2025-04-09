use serde::{Deserialize, Serialize};
use sqlx::FromRow;


//Represents a query with a user's ID to get their database
#[derive(Deserialize)]
pub struct UserQuery {
    pub user_id: String,
}

//Represents a single season
#[derive(Clone, FromRow, Debug, Deserialize, Serialize)]
pub struct Season {
    pub id: i64,
    pub number: i32,
}

//Represents a single episode, attached to a season
#[derive(Clone, FromRow, Debug, Deserialize, Serialize)]
pub struct Episode {
    pub id: i64,
    pub season_id: i64,
    pub number: i32,
    pub title: String,
}

//Represents a single speaker
#[derive(Clone, FromRow, Debug, Deserialize, Serialize)]
pub struct Speaker {
    pub id: i64,
    pub name: String,
}

//Represents a single line attached to a speaker, episode, and season
#[derive(Clone, FromRow, Debug, Deserialize, Serialize)]
pub struct Line {
    pub id: i64,
    pub season_id: i64,
    pub episode_id: i64,
    pub speaker_id: Option<i64>,
    pub speaker_name: Option<String>,
    pub line_number: i32,
    pub content: String,
}

//Represents a search query for a specific phrase
#[derive(Deserialize)]
pub struct SearchPhrasesQuery {
    pub phrase: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub speaker: Option<i64>,
    pub similar_search: Option<bool>,
}

//Represents a query to get a random line from the database
#[derive(Deserialize)]
pub struct RandomLineQuery {
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub speaker: Option<i64>,
}