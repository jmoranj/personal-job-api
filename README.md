# personal-job-api

Rust backend for [personal-job-dash](https://github.com/jmoranj/personal-job-dash). It finds remote software and AI engineering jobs you can take from Brazil, ranks them against your profile, and stores what you did with each one in Postgres.

Built with axum, sqlx, reqwest and Postgres (in Docker).

## Layout

| Path | What |
|---|---|
| `src/main.rs` | HTTP API, background refresh loop |
| `src/sources.rs` | One fetcher per job board, mapped to a common shape |
| `src/filter.rs` | Brazil eligibility rules and match scoring |
| `src/discovery.rs` | Finds company career boards to watch |
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
| GET | `/api/companies` | Company career boards being watched, with their last result. |
| POST | `/api/discover` | Looks for new company boards now (also runs daily). |

## Tune it

Edit `config.toml` and restart:

- `title_keywords` / `exclude_title`: which titles count as your kind of role.
- `profile.skills`: skill → weight. This drives the Match score.
- `companies`: Greenhouse, Lever and Ashby boards that are always watched, on top of the ones discovery finds.

## Company discovery

Small and medium companies post on their own ATS pages, not LinkedIn. Discovery finds those pages so the search can watch them:

1. **Job links:** ATS links (Greenhouse, Lever, Ashby, Workable, Recruitee, Teamtailor, Personio) found in jobs already fetched.
2. **Directories:** [remoteintech](https://remoteintech.company) companies hiring worldwide or in the Americas, and Y Combinator companies that are hiring remotely ([yc-oss](https://github.com/yc-oss/api)).
3. **Probing:** for directory names without a known page, slug guesses are tried on Greenhouse, Lever, Ashby and Workable. A page counts only if it has at least one open job. Misses are retried after 30 days.

Discovery runs a minute after start-up, then daily, and starts a new search when it adds pages. A page with no matching roles for 30 days stops being watched. The first run found about 410 pages.

## Sources

Remotive, Remote OK, Himalayas, Jobicy, We Work Remotely, Arbeitnow, Hacker News "Who is hiring?", the company boards in the config, and every board discovery adds. Remote OK, Jobicy and Remotive ask for attribution. Every job links back to its original posting.

## Brazil filter

Each job gets one label, decided in `src/filter.rs`:

- **yes:** the location names Brazil, LATAM, the Americas or worldwide, or the description clearly says so.
- **maybe:** remote, with no country rule stated.
- **no:** restricted to other countries, or the posting says something like "US only" or "authorized to work in the US".

## Eligibility eval (rules vs Claude)

The keyword rules leave many jobs as "maybe". `src/ai.rs` asks Claude (`claude-opus-5-5`, low effort, JSON-schema output, cached system prompt) to decide those. Before it is switched on, it is measured against hand labels:

```bash
cargo run -- eval sample 150   # 50 each of rules-yes / maybe / no -> evals/eligibility.jsonl
cargo run -- eval label        # label in the terminal: y / m / n, d = full text, s = skip, q = save and quit
cargo run -- eval run          # writes evals/REPORT.md
```

**Two ways to call Claude** (`src/ai.rs`):

| Backend | Needs | Cost |
|---|---|---|
| `api` | `ANTHROPIC_API_KEY` in `.env` | Billed per token (~$0.02–0.04 per job on Opus 5.5) |
| `claude-code` | Claude Code installed and logged in | Uses your Claude Code subscription limits; runs `claude -p` per job, slower |

Claude Code is the default. Set `LLM_BACKEND=api` (with the key) to use the API instead.

The report compares keyword rules, Claude alone, and rules + Claude on "maybe" by accuracy, "yes" precision and recall, jobs wrongly hidden as "no", and cost per job. Claude's answers are cached in `evals/predictions-<model>.jsonl`, so re-running the report is free.

### Claude in the search

After every search, Claude checks up to `[ai] max_per_run` (default 100) jobs the rules left as "maybe", best-scoring first, and its answer becomes the job's eligibility. The rules' own answer is kept in `rules_eligibility`, and a later search never overwrites Claude's answer while the rules still say "maybe". The dashboard marks these jobs "· Claude" and shows Claude's reason. Turn it off with `[ai] enabled = false`.
