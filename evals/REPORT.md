# Eligibility eval — 2026-10-28

150 hand-labeled jobs. Model: `claude-opus-5-5`, via Claude Code (claude -p).

> **Caveat:** 150 of 150 labels were written by an AI assistant (also Claude), not a human. Agreement between Claude and these labels partly measures Claude agreeing with itself; treat the Claude numbers as an upper bound until a human reviews them.

| Classifier | Accuracy | "Yes" precision | "Yes" recall | Wrongly says "no" | Undecided (maybe) |
|---|---|---|---|---|---|
| Keyword rules | 68% | 94% | 96% | 0 | 33% |
| Claude | 95% | 96% | 100% | 0 | 8% |
| Rules + Claude on maybe | 95% | 92% | 100% | 0 | 4% |

**Wrongly says "no"** = jobs actually open to Brazil that would be hidden. It's the costliest mistake.

## Confusion (rows = label, columns = prediction)

**Keyword rules**

| | yes | maybe | no |
|---|---|---|---|
| yes | 47 | 2 | 0 |
| maybe | 1 | 7 | 2 |
| no | 2 | 41 | 48 |

**Claude**

| | yes | maybe | no |
|---|---|---|---|
| yes | 49 | 0 | 0 |
| maybe | 2 | 7 | 1 |
| no | 0 | 5 | 86 |

## Claude disagreements

- **Software Engineer - Future Openings** (FlutterFlow): label no, Claude maybe — Conflicting: location says "Remote - Global" and "15 countries", but description says "based anywhere in the United States."
- **Senior Staff Software Engineer, Partner Integrations** (Flex): label maybe, Claude yes — Location is "Remote - Global" and the company says it has "employees across the US, Australia, Canada, and South America."
- **Senior Data Warehouse Engineer** (Fingerprint): label maybe, Claude yes — "Globally dispersed, 100% remote company... people can join our workforce from almost any country"; only a small number of countries are excluded.
- **Software Engineer** (Virta Health): label maybe, Claude no — Hiring is restricted to US states ("Corporate roles: We currently do not hire in the following states: AK, DE..."), and pay is in USD by location.
- **Senior Full Stack Engineer** (Bounteous): label no, Claude maybe — Location is listed only as "Mexico". The description never mentions remote work or whether candidates must live there, and doesn't mention Brazil or LATAM.
- **Senior Full Stack Engineer** (LiveFlow): label no, Claude maybe — The only location given is "New York, New York". It doesn't say remote, on-site, or which countries they hire from.
- **Senior Software Engineer, Core Platform** (Reddit): label no, Claude maybe — The location is San Francisco and the posting is tagged "#LI-Remote" with US pay and a 401k, but it names no country requirement.
- **Staff Design Engineer - Americas** (Ashby): label no, Claude maybe — The title says "Americas", but the location lists only "Remote - US" and US cities. It's unclear whether Brazil is allowed.

## Cost of this run

150 new calls, $3.652 total, $0.0243 per job at API list price (396162 input, 184074 cached, 22365 output tokens). Run through Claude Code, so it used subscription limits, not money.
