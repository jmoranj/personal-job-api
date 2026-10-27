# personal-job-api

Rust backend for [personal-job-dash](https://github.com/jmoranj/personal-job-dash). It finds remote software and AI engineering jobs you can take from Brazil, ranks them against your profile, and stores what you did with each one in Postgres.

Built with axum, sqlx, reqwest and Postgres (in Docker).

## Layout

| Path | What |
|---|---|
| `src/main.rs` | HTTP API, background refresh loop |
| `src/sources.rs` | One fetcher per job board, mapped to a common shape |
| `src/filter.rs` | Brazil eligibility rules and match scoring |
| `src/config.rs` | Loads `config.toml` |
| `src/infra/db.rs` | Postgres adapter (sqlx queries) |
| `infra/compose.yml` | Local Postgres in Docker |
| `migrations/` | SQL schema |

## Run

Clone both repos side by side:

```
workspace/
├── personal-job-api/
└── personal-job-dash/
```

```bash
cp .env.example .env
```

```bash
docker compose -f infra/compose.yml up -d
```

```bash
cargo run
```

The API listens on http://localhost:8787. It also serves the built dashboard from `../personal-job-dash/dist` (override with `FRONTEND_DIST`). It searches every source on start, then every `refresh_hours`.

## API

| Method | Path | What it does |
|---|---|---|
| GET | `/api/jobs?status=&eligibility=&q=` | List jobs. Defaults: status `new`, blocked jobs hidden. Use `all` to remove a filter. |
| PATCH | `/api/jobs/{id}` | Body `{ "status"?, "notes"? }`. Status is one of new, saved, applied, interviewing, offer, rejected, dismissed. |
| GET | `/api/stats` | Counts per status, jobs new in the last 24h, and the latest run per source. |
| POST | `/api/refresh` | Starts a search of all sources now. |

## Tune it

Edit `config.toml` and restart:

- `title_keywords` / `exclude_title`: which titles count as your kind of role.
- `profile.skills`: skill → weight. This drives the Match score.
- `companies`: Greenhouse, Lever and Ashby board slugs to watch directly.

## Sources

Remotive, Remote OK, Himalayas, Jobicy, We Work Remotely, Arbeitnow, Hacker News "Who is hiring?", and the company boards in the config. Remote OK, Jobicy and Remotive ask for attribution. Every job links back to its original posting.

## Brazil filter

Each job gets one label, decided in `src/filter.rs`:

- **yes:** the location names Brazil, LATAM, the Americas or worldwide, or the description clearly says so.
- **maybe:** remote, with no country rule stated.
- **no:** restricted to other countries, or the posting says something like "US only" or "authorized to work in the US".
