# Eligibility eval — 2026-10-28

150 hand-labeled jobs. Model: `claude-opus-5-5`, via Claude Code (claude -p).

> **Caveat:** 130 of 150 labels were written by an AI assistant (also Claude), not a human; 20 more were decided by a human from AI-extracted evidence. Agreement between Claude and AI-written labels partly measures Claude agreeing with itself, so treat the Claude numbers as an upper bound. The human-reviewed subset is scored separately below.

| Classifier | Accuracy | "Yes" precision | "Yes" recall | Wrongly says "no" | Undecided (maybe) |
|---|---|---|---|---|---|
| Keyword rules | 67% | 96% | 94% | 0 | 33% |
| Claude | 97% | 100% | 100% | 0 | 8% |
| Rules + Claude on maybe | 95% | 96% | 100% | 0 | 4% |

**Human-reviewed subset only (20 hardest jobs):**

| Classifier | Accuracy | "Yes" precision | "Yes" recall | Wrongly says "no" | Undecided (maybe) |
|---|---|---|---|---|---|
| Keyword rules | 65% | 83% | 83% | 0 | 45% |
| Claude | 75% | 100% | 100% | 0 | 60% |

**Wrongly says "no"** = jobs actually open to Brazil that would be hidden. It's the costliest mistake.

## Confusion (rows = label, columns = prediction)

**Keyword rules**

| | yes | maybe | no |
|---|---|---|---|
| yes | 48 | 3 | 0 |
| maybe | 0 | 6 | 3 |
| no | 2 | 41 | 47 |

**Claude**

| | yes | maybe | no |
|---|---|---|---|
| yes | 51 | 0 | 0 |
| maybe | 0 | 8 | 1 |
| no | 0 | 4 | 86 |

## Claude disagreements

- **Software Engineer - Future Openings** (FlutterFlow): label no, Claude maybe — Conflicting: location says "Remote - Global" and "15 countries", but description says "based anywhere in the United States."
- **Senior Full Stack Engineer** (Bounteous): label no, Claude maybe — Location is listed only as "Mexico". The description never mentions remote work or whether candidates must live there, and doesn't mention Brazil or LATAM.
- **Senior Back-End/MLOps Engineer - Real-Estate (DR)** (Truelogic): label maybe, Claude no — Title is tagged "(DR)" and location is Santo Domingo, so it targets the Dominican Republic. Truelogic posts separate listings per country.
- **Senior Full Stack Engineer** (LiveFlow): label no, Claude maybe — The only location given is "New York, New York". It doesn't say remote, on-site, or which countries they hire from.
- **Senior Software Engineer, Core Platform** (Reddit): label no, Claude maybe — The location is San Francisco and the posting is tagged "#LI-Remote" with US pay and a 401k, but it names no country requirement.
