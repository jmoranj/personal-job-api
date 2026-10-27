mod config;
mod db;
mod filter;
mod sources;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::config::Config;
use crate::filter::Matcher;

struct AppState {
    pool: PgPool,
    config: Config,
    client: reqwest::Client,
    refreshing: AtomicBool,
    last_refresh: tokio::sync::RwLock<Option<DateTime<Utc>>>,
}

type Shared = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "job_radar=info".into()))
        .init();

    let config = Config::load(&std::env::var("CONFIG").unwrap_or_else(|_| "config.toml".into()))?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&std::env::var("DATABASE_URL")?)
        .await?;
    sqlx::migrate!().run(&pool).await?;

    let client = reqwest::Client::builder()
        .user_agent("job-radar/0.1 (personal job search)")
        .timeout(Duration::from_secs(45))
        .build()?;

    let state = Arc::new(AppState {
        pool,
        config,
        client,
        refreshing: AtomicBool::new(false),
        last_refresh: Default::default(),
    });

    let bg = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(bg.config.refresh_hours.max(1) * 3600));
        loop {
            tick.tick().await;
            refresh(bg.clone()).await;
        }
    });

    let dist = std::env::var("FRONTEND_DIST").unwrap_or_else(|_| "../personal-job-dash/dist".into());
    let app = Router::new()
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{id}", patch(update_job))
        .route("/api/stats", get(stats))
        .route("/api/refresh", post(trigger_refresh))
        .fallback_service(ServeDir::new(&dist).fallback(ServeFile::new(format!("{dist}/index.html"))))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8787".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    tracing::info!("job radar on http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn refresh(state: Shared) {
    if state.refreshing.swap(true, Ordering::SeqCst) {
        return;
    }
    let started = Utc::now();
    let matcher = Matcher::new(&state.config.profile);
    let results = sources::fetch_all(&state.client, &state.config.companies).await;

    for r in results {
        let (fetched, kept, error) = match r.jobs {
            Ok(jobs) => {
                let mut kept = 0;
                for job in &jobs {
                    if job.title.is_empty() || job.url.is_empty() {
                        continue;
                    }
                    if let Some(verdict) = matcher.judge(job) {
                        match db::upsert(&state.pool, job, &verdict).await {
                            Ok(()) => kept += 1,
                            Err(e) => tracing::warn!("saving {} failed: {e}", job.title),
                        }
                    }
                }
                (jobs.len(), kept, None)
            }
            Err(e) => {
                tracing::warn!("{} failed: {e:#}", r.source);
                (0, 0, Some(format!("{e:#}")))
            }
        };
        tracing::info!("{}: {fetched} fetched, {kept} kept", r.source);
        if let Err(e) = db::record_run(&state.pool, &r.source, started, fetched, kept, error).await {
            tracing::warn!("recording run failed: {e}");
        }
    }

    *state.last_refresh.write().await = Some(Utc::now());
    state.refreshing.store(false, Ordering::SeqCst);
}

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn list_jobs(State(s): State<Shared>, Query(f): Query<db::JobFilter>) -> ApiResult<Vec<db::Job>> {
    db::list(&s.pool, &f).await.map(Json).map_err(internal)
}

#[derive(Deserialize)]
struct JobPatch {
    status: Option<String>,
    notes: Option<String>,
}

async fn update_job(State(s): State<Shared>, Path(id): Path<String>, Json(p): Json<JobPatch>) -> Result<StatusCode, (StatusCode, String)> {
    if let Some(st) = &p.status {
        if !db::STATUSES.contains(&st.as_str()) {
            return Err((StatusCode::BAD_REQUEST, format!("unknown status {st}")));
        }
    }
    match db::update(&s.pool, &id, p.status.as_deref(), p.notes.as_deref()).await.map_err(internal)? {
        0 => Err((StatusCode::NOT_FOUND, "job not found".into())),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

#[derive(Serialize)]
struct Stats {
    statuses: Vec<db::Count>,
    new_last_24h: i64,
    sources: Vec<db::SourceRun>,
    refreshing: bool,
    last_refresh: Option<DateTime<Utc>>,
    refresh_hours: u64,
}

async fn stats(State(s): State<Shared>) -> ApiResult<Stats> {
    Ok(Json(Stats {
        statuses: db::status_counts(&s.pool).await.map_err(internal)?,
        new_last_24h: db::new_today(&s.pool).await.map_err(internal)?,
        sources: db::latest_runs(&s.pool).await.map_err(internal)?,
        refreshing: s.refreshing.load(Ordering::SeqCst),
        last_refresh: *s.last_refresh.read().await,
        refresh_hours: s.config.refresh_hours,
    }))
}

async fn trigger_refresh(State(s): State<Shared>) -> StatusCode {
    tokio::spawn(refresh(s));
    StatusCode::ACCEPTED
}
