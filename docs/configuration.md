# Configuration

Settings come from, in order of precedence (last wins):

1. Built-in defaults: `defaults/zontologize.toml` and `defaults/rules.toml` (the full key list).
2. The config directory (`--config DIR` or `ZONTOLOGIZE_CONFIG`). Only set what you change.
   `zontologize.toml` merges key by key, tables included (`[providers.*]`, `[embed.models.*]`,
   `resolve.similar.weights`), with one exception: `[scan.kinds]` replaces the default kinds as a
   whole, so list every kind you want to read. Every other file (`rules.toml`, `prompts/*.md`)
   replaces the built-in file as a whole: a `rules.toml` without `[ocr_confusions]` has no OCR
   confusions, and one with `[[page_types]]` needs `pages.head_chars`.
3. Environment: `ZONTOLOGIZE_<SECTION>__<KEY>`, for example `ZONTOLOGIZE_EXTRACT__THREADS=4`.
   Deeper keys add another `__` (`ZONTOLOGIZE_PROVIDERS__OLLAMA__THINK=true`). A value is read
   as TOML, except for keys that hold text, which take it as is. A key of one `[[sources]]`
   entry takes its name: `ZONTOLOGIZE_SOURCES__<NAME>__<KEY>`, for example
   `ZONTOLOGIZE_SOURCES__ARCHIVE__ROOT=/data/archive`; the entry must exist.
4. Command-line flags.

An unknown key or bad value stops the run before it touches the database.

## Environment variables

| Variable | Use |
|---|---|
| `DATABASE_URL` or `PGHOST`, `PGUSER`, `PGPASSWORD`, ... | PostgreSQL connection |
| `OPENROUTER_API_KEY` | `openrouter` provider |
| `OLLAMA_HOST` | Ollama server (default `http://localhost:11434`) |
| `CLAUDE_CODE_BIN` | `claude` binary (default: on `PATH`) |
| `HF_HOME`, `HF_HUB_CACHE` | Hugging Face model cache |

Secrets never go in config files.

The `claude` binary gets a cleared environment with only `PATH`, `HOME`, `USER`, `LOGNAME`,
`LANG`, `LC_*`, `TZ`, `TMPDIR`, the `XDG_*` folders, `CLAUDE_CONFIG_DIR`, the proxy variables
and the CA certificate variables (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`). It
logs in with its own stored login; `ANTHROPIC_API_KEY` and database variables are not passed.

## zontologize.toml

In the tables, `0` means "all cores", "no limit" or "auto" unless noted.

### Sources

```toml
[[sources]]
name = "projects"          # unique
kind = "folder"            # folder (default) or table
root = "/data/projects"    # absolute or relative to the config directory
dataset = "current"        # optional label
ignore = ["**/*.bak"]      # optional globs

[[sources]]
name = "register"
kind = "table"
view = "files.register"    # schema.view, read only
root = "/data/register"    # files named by the view must lie under it
```

A `table` source reads its file list from a view instead of walking `root`. The view needs the
columns `id` (any type, sorted as text), `path` (the path stored for the document), `file` (the
file to read, relative to `root` or absolute under it; null: `path`), `sha256` (null: the file is
hashed), `size` and `modified` (timestamp). `ignore` and `[scan.kinds]` apply to `path`.

### Core

| Key | Default | Meaning |
|---|---|---|
| `database.schema` | `"zontologize"` | Schema to write |
| `database.graph` | `"zontologize_graph"` | AGE graph that `graph sync` rebuilds; must differ from the schemas |
| `database.api_schema` | `"zontologize_api"` | Schema of the views for apps, made by `db migrate` |
| `database.max_connections` | `8` | Connection pool size |
| `database.decisions` | `""` | `schema.view` with human decisions for `decisions apply` (see `docs/consumers.md`); empty: none |
| `ontology.files` | `[]` | Turtle files, relative to the config directory |
| `ontology.languages` | `["en"]` | Label languages, first preferred |
| `ontology.include` / `exclude` | `[]` | Root classes to select / leave out |
| `ontology.document_class` | `""` | Class given to every document |
| `ocr.languages` | `["en"]` | OCR languages |
| `ocr.model_tier` | `"small"` | `small` or `medium` |
| `scan.ignore` | lock and system files | Globs never read |
| `scan.batch_size` | `1000` | Files per round of the walk |
| `scan.hash_threads` | `0` | Threads that hash files |
| `scan.kinds.<kind>.extensions` | per kind | File extensions per kind |
| `scan.kinds.<kind>.max_mb` | `0` | Skip larger files |
| `chunk.max_chars` / `overlap` | `2000` / `200` | Chunk size |
| `chunk.text_search` | `"simple"` | PostgreSQL text search config |

### Extraction

| Key | Default | Meaning |
|---|---|---|
| `extract.workers` | `0` | Worker processes (0: one per 4 cores) |
| `extract.threads` | `0` | Threads per worker |
| `extract.worker_memory_mb` | `0` | Memory per worker (0: split evenly) |
| `extract.timeout_secs` | `300` | Per file |
| `extract.format` | `"markdown"` | `markdown` or `plain`. PDFs and images are always plain, and their pages also get the page size and word boxes |
| `extract.embedded_depth` | `0` | How deep to open attachments |
| `extract.max_embedded_mb` | `10` | Largest attachment opened |
| `extract.worker_files` | `500` | Files a worker process handles before it is replaced |
| `extract.ocr_embedded_images` | `false` | OCR images inside documents |

### Models

Names are `provider:model`. Providers: `claude-code`, `openrouter`, `ollama`. Embeddings are
local only (`local:<name>`, see [embeddings.md](embeddings.md)).

| Key | Default |
|---|---|
| `models.fit` | `"claude-code:sonnet"` |
| `models.extract` | `"claude-code:sonnet"` |
| `models.classify` | `"claude-code:haiku"` |
| `models.attributes` | `"claude-code:sonnet"` |
| `models.curate` | `"claude-code:sonnet"` |
| `models.eval` | `"claude-code:opus"` |
| `models.embed` | `"local:arctic-embed-m-v2.0"` |
| `models.chars_per_token` | `2.0` |

`models.eval` makes and judges the gold data (`eval draft`, `review sample --judge`); it should
be stronger than the extract model, for example `claude-code:opus`.

`models.chars_per_token` turns characters into tokens for the cost estimate until calls have been
recorded.

Per provider (`[providers.claude-code]`, `[providers.openrouter]`, `[providers.ollama]`):
`concurrency`, `timeout_secs`; plus `max_window_use` (claude-code), `base_url` (openrouter),
`think` (ollama), `poll_minutes` (openrouter: how often to check the key when funds run out).

`providers.claude-code.config_dirs` (default `[]`) lists Claude Code config dirs, one per
logged-in account, e.g. `["~/.claude-a", "~/.claude-b"]` (log each in with
`CLAUDE_CONFIG_DIR=~/.claude-a claude /login`). Each request runs with one of them as
`CLAUDE_CONFIG_DIR`; an account that hits its usage limit or `max_window_use` is paused until its
reset while the job goes on with the others, and the job waits only when every account is
paused. `account_window_use` (default `{}`) sets the cap per account, e.g.
`{ "~/.claude-b" = 0.95 }`; the others use `max_window_use`. Claude Code no longer reports the
window use with each reply, so before sending, and again every five minutes, each account's use is read
from the OAuth usage endpoint that Claude Code's own `/usage` reads, with the token in the
account's `.credentials.json`. An account without that file (the token kept elsewhere) is not read. When a reading fails (the endpoint rate-limits), the last reading of the current window is kept; without one, an account with a cap below 1 pauses until
a reading works again; at 1 it sends until it is rejected. An account that reports paid extra
usage (`isUsingOverage`) is paused until its reset. `concurrency` applies per account. `models check` and `status` name each account. Empty
means the current `CLAUDE_CONFIG_DIR`, or Claude Code's default when unset. Switching between
personal Max accounts to get past usage limits may break Anthropic's consumer terms; check
before relying on it.

| Key | Default | Meaning |
|---|---|---|
| `budget.max_usd` | `0` | Spend cap per job |
| `budget.max_requests` | `0` | Request cap per job |
| `budget.calibration_calls` | `200` | Recorded calls used to estimate tokens per request |
| `budget.default_output_tokens` | `300` | Output tokens per request until calls are recorded |
| `llm.retry_backoff_secs` | `2` | First wait before a retry; doubles per attempt |
| `llm.wait_margin_secs` | `60` | Extra wait after a usage window resets |
| `llm.unknown_reset_wait_minutes` | `30` | Wait when a limit gives no reset time |
| `llm.progress_secs` | `60` | Seconds between progress lines of a model job |

### GPU

The default build runs on the CPU. A build with `cargo build --release --features gpu` can run the
local embedding model and the OCR of `scan` on an NVIDIA GPU with CUDA 13: set
`gpu.provider = "cuda"`. The CUDA 13
runtime, cuBLAS and cuDNN 9 must be on `LD_LIBRARY_PATH`; under WSL also `/usr/lib/wsl/lib`
(`libcuda.so`). When the device is not available, embed goes on on the CPU and says so, while
OCR stops with an error; `models check` reports the execution provider and fails when CUDA is set
but not available. On an RTX A1000 embedding runs about 7 times faster than on the CPU, and
`embed.batch_size` 8 and 32 give the same rate. Every extract thread loads its own OCR model on
the GPU (about 1.5 GB each), so keep `extract.workers` × threads per worker at 2 on a card with
8 GB, for example `workers = 1` and `threads = 2`: more runs out of GPU memory or gets slower.
CPU and GPU OCR give the same text (CER 2.26% on both for the test data).

| Key | Default | Meaning |
|---|---|---|
| `gpu.provider` | `"cpu"` | `cpu` or `cuda`; `cuda` needs a build with `--features gpu` |
| `gpu.device` | `0` | CUDA device number |

### Stage tuning

The defaults work for most data. Commonly changed:

| Key | Default | Meaning |
|---|---|---|
| `fit.sample` | `1000` | Chunks sent to `ontology fit` |
| `fit.out` | `"fit"` | Report folder |
| `fit.seed` / `llm_extract.seed` | `"zontologize"` | Sample seed |
| `fit.min_chars` | `200` | Shortest chunk sampled |
| `fit.unmatched_share` / `llm_extract.unmatched_share` | `0.3` | Share of the sample taken from chunks no class name matched |
| `fit.max_shortlist` / `llm_extract.max_shortlist` | `30` | Most classes offered per chunk |
| `fit.min_evidence` / `llm_extract.min_evidence` | `3` | Chunks needed before a new class or term is proposed |
| `fit.proposal_namespace` | `"urn:zontologize:proposal#"` | IRI prefix of proposed classes |
| `llm_extract.sources` / `doc_types` / `paths` | `[]` | Limit what `extract` sends |
| `llm_extract.exclude` | `[]` | Paths never sent |
| `llm_extract.min_chars` | `200` | Shortest chunk sent |
| `llm_extract.confidence` | `0.7` | Confidence of model facts |
| `llm_extract.max_per_document` | `3` | Most chunks per document in a sample |
| `llm_extract.min_letter_share` / `embed.min_letter_share` | `0.5` | Chunks with fewer letters are skipped, by every `extract` and `attributes` run (not only `--sample`); so is a chunk whose text repeats an earlier chunk. The reports count both |
| `llm_extract.min_ocr_confidence` / `embed.min_ocr_confidence` | `0.6` | Chunks on OCR pages below this are skipped, as above |
| `llm_extract.skip_paths` | `[]` | Globs of old copies (`**/*_old.*`): read, chunked and searchable, but never sent by `extract` or `attributes`; counted in the reports |
| `llm_extract.repeated_line_files` | `10` | A line (with letters) found in more files than this is left out of the text sent to `extract` (0: off); the stored chunk is unchanged |
| `llm_extract.quote_headers` | `[]` | Regexes for the header of a quoted mail; the text from the first match on is not sent when most of its lines are also in another file |
| `llm_extract.min_tag_words` | `2` | A model entity named by a bare tag (digits, no word of 3 letters) is kept only when its line has its class name or this many other words; otherwise it becomes a mention of the document |
| `llm_extract.resend_share` | `0.2` | Share of chunks sent again without `--yes` |
| `llm_extract.out` | `"extract"` | Report folder |
| `llm_extract.max_known` | `0` | Most known entities offered per chunk as `rN` (0: `llm_extract.max_shortlist`), in this order: rule hits in the chunk, rule hits on its document (project, customer, site from the path), things one rule relation away from those, rule hits in the other chunks of the content; each cluster once, as its canonical row. Model output never feeds this list |
| `llm_extract.threads` | `0` | Threads that build requests (0: `scan.hash_threads`) |
| `attributes.confidence` | `0.7` | Confidence of model values until reviews set it |
| `attributes.context_chars` | `400` | End of the previous chunk sent as context, for table headers |
| `attributes.max_text_chars` | `120` | Longest text value kept |
| `attributes.max_properties` | `30` | Most properties asked per chunk; properties named in the text go first |
| `attributes.max_subjects` | `50` | Most known things offered per chunk, in entity order |
| `attributes.out` | `"attributes"` | Report folder |
| `attributes.sources` / `doc_types` / `paths` | `[]` | Limit what `attributes` sends (empty: the `llm_extract` value) |
| `attributes.seed` | `""` | Sample seed (empty: `llm_extract.seed`) |
| `attributes.resend_share` | unset | Share of chunks sent again without `--yes` (unset: `llm_extract.resend_share`) |
| `attributes.threads` | `0` | Threads that build requests (0: `scan.hash_threads`) |
| `attributes.min_line_words` | `2` | Only lines with this many words (2 or more letters) count when a chunk is looked at for a property name or unit |
| `attributes.unit_symbols` | `12` | Unit symbols listed per quantity kind in the prompt |
| `classify.head_chars` | `1500` | Text sent per part |
| `classify.min_chars` | `50` | Parts with less text are not sent |
| `classify.confidence` | `0.7` | Confidence of a model document type |
| `curate.confidence` | `0.9` | Curate decisions at or above this are `auto`, below it `proposed` (shown in `review merges`) |
| `curate.max_members` | `8` | Largest cluster of proposed same-as links sent to the curate model; larger ones, and names held by more entities in disjoint classes, are skipped and counted |
| `curate.context_chars` | `100` | Characters of text before and after each mention snippet |
| `curate.max_shortlist` | `30` | Most classes offered per cluster: the members' classes, their parents, siblings and children |
| `curate.definition_chars` | `200` | Longest class definition in the curate prompt |
| `curate.out` | `"curate"` | Report folder |
| `curate.not_in_class` | `{}` | Class name (prefixed or `<IRI>`) to a list of regex patterns a name of that class (or a subclass) cannot match; a hit sends the entity to curate, for example legal forms under a site or person class |
| `curate.folder_classes` | `[]` | Classes (e.g. a project class) whose model-made entities are sent to curate together with the rule-made entity of that class that the documents mentioning them belong to, so curate joins aliases to it, picks another class or drops them. Curate never renames, reclasses or drops that rule-made entity |
| `curate.gazetteer` | `""` | Generated gazetteer (CSV, same columns as `gazetteer/*.csv`) written after `curate` and read by the rules of the next run; empty: none |
| `curate.gazetteer_documents` | `3` | Documents a model-made cluster must be named in before it enters the generated gazetteer |
| `curate.gazetteer_classes` | `[]` | Classes the generated gazetteer takes |
| `fit.definition_chars` / `llm_extract.definition_chars` / `classify.definition_chars` | `200` | Longest class definition in a prompt |
| `review.sample` | `50` | Facts shown by `review sample` |
| `review.min_verdicts` | `30` | Reviews of a class or property before they set its confidence (0: never) |
| `review.checked` | `0.8` | Confidence from which a model fact is in the `checked` tier of the API views; below it is a `proposal` |
| `review.rule_classes` | `[]` | Classes a model alone cannot make: an entity of one of them that only the model made is a `proposal` in the API views until it joins a rule-made entity or a person confirms it |
| `run.summary` | `"summary.md"` | Where `zontologize run` writes its summary (Markdown, plus a `.json` beside it with the numbers of the last finished run, for the change column) |
| `run.set` | `""` | Gold set (`eval draft --set`) that `run` scores against; with it, `run` also runs the judge (`review sample --judge`); empty: the run ends without numbers |
| `run.targets` | `{ found = 0.9, duplicates = 0.05, class_accuracy = 0.9, mentions = 0.9, relations = 0.9, attributes = 0.85, catch_rate = 0.9 }` | Targets for the verdicts in the run summary: good when met, fair within 10 points, poor beyond |
| `eval.dir` | `"eval"` | Folder of the gold sets of `eval draft` (`<dir>/<set>/v<n>/`) and of `judge-report.md`, relative to the config dir |
| `eval.summary` | `""` | Results summary that `eval entities` and `review sample --judge` write their section into; empty: not written |
| `eval.seed` | `"eval"` | Seed of the gold samples and of the judge's draw |
| `eval.docs_per_kind` | `3` | Documents per document kind and first-level folder in each of the two world samples |
| `eval.min_doc_chars` / `max_doc_chars` | `200` / `6000` | Documents with less text are left out; text sent per document |
| `eval.request_chars` | `60000` | Most document text per gold request |
| `eval.questions` | `25` | Questions per set made by `eval draft --what questions` |
| `eval.finds` | `40` | Search queries per set made by `eval draft --what finds` |
| `eval.type_docs` | `300` | Documents labelled with a class by `eval draft --what types` |
| `eval.attribute_chunks` | `300` | Chunks labelled by `eval draft --what attributes` |
| `eval.judge_batch` / `judge_context` | `10` / `600` | Claims per judge request; characters of text around a fact |
| `eval.judge_mentions` / `judge_relations` / `judge_attributes` | `100` / `100` / `100` | Facts per kind the judge marks in `review sample --judge` |
| `eval.planted` | `20` | Planted wrong facts per kind mixed into the judge's sample |
| `eval.min_catch_rate` | `0.9` | Share of planted errors the judge must catch, or its report is marked failed |
| `embed.threads` / `batch_size` | `0` / `8` | Embedding throughput |
| `embed.min_chars` | `50` | Shorter chunks are skipped |
| `embed.max_tokens` | `512` | Longest input in tokens for a model that sets no `max_tokens` |
| `embed.max_aliases` | `5` | Aliases added to an entity's text |
| `embed.out` | `"eval"` | Report folder of `eval embed` |
| `embed.eval_sample` / `eval_max_expected` | `500` / `20` | Questions made by `eval embed`, most expected chunks per question |
| `embed.cache_dir` | `""` | Model cache (empty: standard HF cache) |
| `embed.progress_secs` | `30` | Seconds between progress lines |
| `resolve.strip_words` | `[]` | Whole words ignored when names are compared and in the keys of extracted entities, such as legal forms; after a change the next `extract` re-keys extracted entities and folds rows that end on one key |
| `resolve.fuzzy_score` / `embed_score` | `0.92` / `0.9` | Match thresholds |
| `resolve.fuzzy_auto` | `1.01` (off) | Fuzzy score from which a match between two names of the same class is `auto` instead of `proposed`, for example 0.97 for OCR and typing variants |
| `resolve.classes` | `[]` | Limit to these classes |
| `resolve.max_candidates` | `5` | Nearest neighbours compared per entity |
| `resolve.block_chars` / `max_block` | `4` / `1000` | Token prefix that groups candidates (every token of 3+ characters, and the acronym of a name); groups larger than `max_block` (common words) are not compared |
| `resolve.similar.property` | `"urn:zontologize#similarTo"` | IRI of similarity links |
| `resolve.similar.tolerance` | `0.2` | Relative gap at which a value no longer counts as close |
| `resolve.similar.min_shared` / `min_score` / `max_links` | `2` / `0.8` / `5` | Values two entities must share, lowest score, links per entity |
| `resolve.similar.weights` | `{}` | Weight per property name, default 1 |

`attributes` also uses `llm_extract.min_chars`.

Batch sizes and threads, rarely changed:

| Key | Default | Meaning |
|---|---|---|
| `scan.extract_batch` / `chunk_batch` / `rules_batch` | `200` / `200` / `100` | Rows read per round by extract, chunk and the text rules |
| `scan.rules_threads` | `0` | Threads of the rules stage (0: `scan.hash_threads`) |
| `chunk.threads` | `0` | Threads of the chunk stage (0: `scan.hash_threads`) |
| `llm_extract.read_batch` / `attributes.read_batch` | `2000` | Chunks read per round when planning |
| `classify.read_batch` | `500` | Documents read per round |
| `embed.read_batch` | `2000` | Rows read per round |
| `resolve.neighbour_batch` | `500` | Entities per nearest-neighbour query |

### Export

| Key | Default | Meaning |
|---|---|---|
| `export.ontology_iri` | `"urn:zontologize:ontology"` | IRI of the `owl:Ontology` that `ontology export` writes |

### Disjoint classes

`owl:disjointWith` between two classes says nothing can be both; it holds for their subclasses
and for classes typed with them (such as CFIHOS equipment classes). `extract` drops an entity
that gives a known thing's name a class disjoint with the known thing's class, and `curate` uses
the same test. A class that sits under two disjoint classes is an error in `ontology check`.

### Entity quality

`zontologize eval entities --set <name>` compares the entities with the world list of the newest
frozen version of a gold set (`--world <file>` takes any list instead, such as the test data's
`data/tests/world.csv`): a CSV with `name,variant,class`, one row per spelling of a known thing
(more columns are ignored). An entity matches a name when its name, key or an alias equals a
variant, ignoring case and extra spaces. Per class it prints `world` (names in the list), `found`
(names with a match) and its share, `dup rate` (found names whose matches are in more than one
cluster, over found), `extra` (clusters beyond the first per found name), `wrong merges`
(clusters holding two or more names) and `class acc` (matches whose class is the listed class or
a subclass of it). A cluster is the `canonical_id` of `resolve`; before `resolve` every entity is
its own cluster. With `eval.summary` set, the table also goes into that file's *Gold list*
section.

### Gold data

`zontologize eval draft --set <name> --what world|questions|finds|types|attributes [--folders a,b]` makes
gold data from the raw text of the documents with `models.eval`, never from the entities. It
writes `<eval.dir>/<name>/v<n>/`: the gold file, `documents.csv` and `report.md`. A version
belongs to one set of documents (a hash of their paths and contents); while the documents stay
the same, a gold file once written is frozen and the command does nothing. Each item carries a
file and a quote; an item whose quote is not in that file's text (ignoring case and spaces) is
dropped.

- `world`: per first-level folder two samples of documents, `docs_per_kind` per document kind.
  The model lists the important things and every spelling; a second call per sample joins
  spellings of one thing. Things found in both samples are gold; the rest is sent to a judging
  call and kept only when it confirms them. `world.csv` has `name,variant,class,file,quote`.
- `questions`: about `eval.questions` questions in the ontology's languages, each with the files
  that answer it, kept only when the model answers it from those files alone. `questions.toml` is
  read by `eval embed --questions <file>`.
- `finds`: about `eval.finds` short search queries as a colleague types them in a search box
  (a number, a name, a type with a project), each with the files it should find, kept only when
  the model confirms from those files alone that they are what the query looks for. Grounded in
  the file's path or text. `finds.toml` has the shape of `questions.toml`.
- `types`: about `eval.type_docs` documents, spread over the folder types, each labelled twice
  (in two orders) with the most specific subclass of `[ontology] document_class`, or `other`;
  labels both passes agree on are gold, the judge decides the rest. `types.csv` (`path`, `class`)
  is read by `zontologize eval types --set <name>`, which prints accuracy and precision and recall
  per class of the run's document classes and writes them to `[eval] summary`.
- `attributes`: a stratified sample of `attribute_chunks` chunks, labelled twice (the second time
  in reverse order) with equipment, property, value, unit and quote; values in one pass only go
  to the judge. Writes `attributes.csv` and the sample in `chunks.csv`.

`report.md` gives the counts, the grounding drops and the agreement rate of the two runs.

### Judged review

`zontologize review sample --judge` lets `models.eval` mark a stratified sample of facts (per
method and class or property; `judge_mentions`, `judge_relations`, `judge_attributes`) right or
wrong from the text around them, mixed with `planted` wrong facts per kind: swapped ends of a
relation, another class, a changed number or unit, a name from another chunk. Facts with a human
verdict are judged first, so the two can be compared. Verdicts go to `fact_reviews` with
`reviewer = 'judge'`; planted ones have `planted = true` and never count as precision. A rerun
only tops up to the configured numbers. The report (`<eval.dir>/judge-report.md`, and the
*Judged precision* section of `eval.summary`) gives precision per kind with its 95% margin and the
share of planted errors caught; under `min_catch_rate` it is marked failed.

## Units and attribute properties

Attribute values need units with conversion factors. Download QUDT's vocabularies (CC BY 4.0)
into the config directory and list them in `ontology.files`:

```sh
curl -L -H "Accept: text/turtle" -o ontology/qudt-unit.ttl https://qudt.org/vocab/unit/
curl -L -H "Accept: text/turtle" -o ontology/qudt-quantitykind.ttl https://qudt.org/vocab/quantitykind/
```

A datatype property becomes an attribute when its domain is selected and its range is a number
(`xsd:decimal`, `xsd:integer`, ...) or text (`xsd:string`, no range). A number with a unit names
its quantity kind; a unit fits when it has the same dimension vector and, when any unit of
that dimension lists the kind with `qudt:hasQuantityKind`, also lists it (or lists no kind).
A property's domain and range may be an `owl:unionOf` of classes. A property without a domain
(or an object property without a range) fits no class and is not extracted. A class fits a
domain or range when it is the bound, a subclass of it, or (on that subclass chain) directly
typed as the bound, so a meta-class such as CFIHOS's equipment class works as a domain; a class
typed as an instance of another class does not inherit that class's domains. Values go only to
the things the chunk mentions, never to the document itself:

```turtle
ex:dischargePressure a owl:DatatypeProperty ;
    rdfs:domain ex:Pump ; rdfs:range xsd:decimal ;
    qudt:hasQuantityKind quantitykind:Pressure ;
    zontologize:similarityTolerance 0.1 ;          # optional, see resolve.similar
    skos:prefLabel "discharge pressure"@en, "persdruk"@nl .
```

Units are matched on `qudt:symbol`, `rdfs:label` and `skos:altLabel`, exact
first, then case-insensitive when only one unit fits. Add the spellings your documents use as
`skos:altLabel` in the local file (`unit:M3-PER-HR skos:altLabel "m3/hr" .`); the
`attributes-report.md` lists the unit texts that matched nothing. Gauge pressure is a unit of
its own with an offset of one atmosphere, so values convert to absolute pascal:

```turtle
ex:BAR_G a qudt:Unit ; qudt:symbol "barg" ; skos:altLabel "bar(g)" ;
    qudt:conversionMultiplier 100000.0 ; qudt:conversionOffset 1.01325 ;
    qudt:hasDimensionVector qkdv:A0E0L-1I0M1H0T-2D0 .
```

## rules.toml

Rules find facts without a model. Class and property names are prefixed names or IRIs.
Path patterns match the path relative to the source root; `{group}` inserts a named group.
`examples/acme/rules.toml` is a full example.

```toml
[doc_types]
invoice = "acme:Invoice"

[[path_rules]]
pattern = '^Projects/(?P<project>P-\d{4}-\d{4})/'
entities = [{ id = "p", class = "acme:Project", key = "{project}" }]
relations = [{ from = "document", to = "p", property = "acme:describes" }]

[[path_rules]]
pattern = '/Invoices/'
doc_type = "invoice"

[[id_patterns]]
class = "acme:Project"
pattern = '\bP-\d{4}-\d{4}\b'
on = "both"                  # path, text or both

[ocr_confusions]
"0" = "Oo"

[[page_types]]               # split one file into parts by page
doc_type = "invoice"
pattern = '(?i)^invoice'

[[name_types]]               # a document's class from its file name; doc_type (the folder) stays
pattern = 'drawing|dwg'      # case-insensitive, on the file name only; the last matching rule wins
class = "acme:Drawing"       # a document class; no match keeps the class of doc_type
exclude = 'comment|review'   # optional: names that are not this document
extensions = ["pdf", "dwg"]  # optional: only these extensions

[pages]
head_chars = 400             # page text searched for a page type
part_property = "acme:partOf"

[[revisions]]                # link later revisions with dcterms:replaces
pattern = '(?P<series>DWG-\d{3}) rev (?P<rev>[A-Z0-9]+)\.'

[[number_keys]]              # resolve: names with a project number join the entity of that key
class = "acme:Project"
pattern = '(?i)^(?:P-?)?(?P<n>\d{4}-\d{4})(?:\s|$)'
key = "P-{n}"

[[number_keys]]              # a second number shape: its own key, never merged with P-{n}
class = "acme:Project"
pattern = '(?i)^SP-?(?P<n>\d{4}-\d{4})(?:-[a-z0-9]+)*(?:\s|$)'
key = "SP-{n}"
link = { property = "acme:sparesOf", key = "P-{n}" }  # resolve: a relation to the entity of this key, from the same digits

[persons]                    # people from mail headers, e-mail addresses and signatures
class = "acme:Person"
sent_by = "acme:sentBy"
sent_to = "acme:sentTo"
works_for = "acme:worksFor"  # from the address's domain, when a gazetteer alias is that domain
closings = ["kind regards", "met vriendelijke groet"]
role_addresses = ["info", "sales"]  # a local part that is, or starts with, one of these words is no person
```

A mail address becomes a person only with a name: from the display name, the text before
`<address>`, or a local part that splits into a name (`jan.devries`). Message-ID, References and
In-Reply-To lines, hex IDs, role addresses and an address used with two different names in one
text give no person. `resolve` pairs people of one organisation or mail domain whose names differ
only in initials or particles (`R. v.d. Sar`, `r vandersar`, `Ruud van der Sar`) with rule
`initials`: `auto` when one of them is keyed by a mail address or is in the gazetteer (and not both
by an address), else `proposed`. A first name alone never pairs.

A path rule entity can carry `props` from the path, for example
`{ id = "p", class = "acme:Project", key = "{project}", props = { year = "{year}" } }`;
an empty value is left out.

A class gets a privacy category with `zontologize:privacyCategory "PERSON"` (upper-case
letters and `_`); `zontologize_api.classes.privacy_category` shows it.

`zontologize db fold-class --from <class> --into <class>` moves the entities of a class that left
the ontology into another class, merging those with the same key.

`zontologize db relocate --plan <file>` follows files that moved on disk to another source or
folder. The plan is tab-separated, one line per file or folder: `from_source`, `from_path`,
`to_source`, `to_path` (more columns and `#` lines are ignored). Documents keep their ids, so
text, chunks, vectors, people and decisions stay; their path rules run again on the next scan.
Every `to_source` must be in `[[sources]]`; a source that is no longer configured and has no
documents left is removed. Move the files first, change `[[sources]]`, then relocate and scan.

## Gazetteer

`gazetteer/*.csv` with header `class,key,name,aliases,lang`. Aliases are separated by `|`.
Names and aliases match as whole words, case-insensitive.

## Prompts

`prompts/<name>.md` replaces a built-in prompt (`fit-*`, `extract-*`, `classify-*`, `attributes-*`, `curate-*`, see
`prompts/`). Variables are written `{name}`. A user prompt must contain the text variable
(`{chunk}`, `{text}` for `classify-user`, `{members}` for `curate-user`). A value that contains `<<<` or `>>>` gets a space
inside the run (`<< <`), so the text cannot close the delimiters.

## questions.toml

Optional test questions for `eval embed` (`eval embed --questions <file>` reads another file, such
as the `questions.toml` of an `eval draft` set):

```toml
[[questions]]
text = "Which drawing shows the pipe section?"
expect = ["Projects/2021/P-2021-0042*/04 Reports/*"]
```
