#!/usr/bin/env python3
"""Builds a bench set: a fixed, stratified sample of documents copied to its own folder, with a
config that runs the whole pipeline in its own schema.

Usage: tools/bench-set/make.py --config config --out bench/v1 \
           [--chunks 300] [--seed bench] [--max-doc-chunks 40]
Reads the documents of the first folder source from the configured database (DATABASE_URL,
read-only), picks documents round-robin over (project folder, document type, letter share)
strata in a seeded order until about --chunks chunks, copies them with their relative paths
and writes <out>/config with schema, graph, API schema, result folders and the source moved.
"""

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

QUERY = """
select d.id, d.path, coalesce(d.doc_type, d.kind, '') as kind,
       count(c.id) as chunks,
       coalesce(avg(length(regexp_replace(c.text, '[^[:alpha:]]', '', 'g'))::float
                / nullif(length(c.text), 0)), 0) as letters
from {schema}.documents d
join {schema}.sources s on s.id = d.source_id and s.name = '{source}'
join {schema}.chunks c on c.content_id = d.content_id
where d.status = 'present'
group by d.id, d.path, d.doc_type, d.kind
"""


def rows(schema: str, source: str) -> list[tuple[int, str, str, int, float]]:
    out = subprocess.run(
        ["psql", os.environ.get("DATABASE_URL", ""), "-AtF", "\t", "-c",
         QUERY.format(schema=schema, source=source)],
        check=True, capture_output=True, text=True,
    ).stdout
    found = []
    for line in out.splitlines():
        id_, path, kind, chunks, letters = line.split("\t")
        found.append((int(id_), path, kind, int(chunks), float(letters)))
    return found


def rank(seed: str, id_: int) -> str:
    return hashlib.sha256(f"{seed}:{id_}".encode()).hexdigest()


def pick(docs, chunks: int, seed: str, max_doc_chunks: int):
    strata: dict[tuple[str, str, bool], list] = {}
    for doc in docs:
        if doc[3] > max_doc_chunks:
            continue
        top = doc[1].split("/", 1)[0]
        strata.setdefault((top, doc[2], doc[4] >= 0.5), []).append(doc)
    for key in strata:
        strata[key].sort(key=lambda d: rank(seed, d[0]))
    order = sorted(strata, key=lambda k: rank(seed, hash_key(k)))
    chosen, total = [], 0
    while total < chunks and any(strata[k] for k in order):
        for key in order:
            if strata[key] and total < chunks:
                doc = strata[key].pop(0)
                chosen.append(doc)
                total += doc[3]
    return chosen, total


def hash_key(key) -> int:
    return int(hashlib.sha256("|".join(map(str, key)).encode()).hexdigest()[:12], 16)


def section(text: str, header: str) -> tuple[int, int]:
    start = text.index(header)
    nxt = re.search(r"^\[", text[start + len(header):], re.M)
    end = start + len(header) + nxt.start() if nxt else len(text)
    return start, end


def config(src: Path, out: Path, source: dict, name: str) -> None:
    dst = out / "config"
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)
    toml = dst / "zontologize.toml"
    text = toml.read_text()
    text = re.sub(r"(?ms)^\[\[sources\]\].*?(?=^\[(?!\[sources))", "", text)
    text = re.sub(r'(?m)^(\w+) = "[^"]*/results/([^"]*)"',
                  lambda m: f'{m.group(1)} = "{out}/results/{m.group(2)}"', text)
    text = re.sub(r'(?m)^set = "[^"]*"', 'set = "bench"', text)
    text = re.sub(r'(?m)^decisions = .*$', 'decisions = ""', text)
    db = (f'[database]\nschema = "zontologize_{name}"\ngraph = "zontologize_{name}_graph"\n'
          f'api_schema = "zontologize_{name}_api"\ndecisions = ""\n\n')
    text = re.sub(r"(?ms)^\[database\]\n.*?(?=^\[)", "", text)
    text = db + text + (f'\n[[sources]]\nname = "{source["name"]}"\nkind = "folder"\n'
                        f'root = "{out / "files"}"\ndataset = "current"\n')
    toml.write_text(text)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--name", default="bench")
    parser.add_argument("--schema", default="zontologize")
    parser.add_argument("--chunks", type=int, default=300)
    parser.add_argument("--seed", default="bench")
    parser.add_argument("--max-doc-chunks", type=int, default=40)
    args = parser.parse_args()
    text = (args.config / "zontologize.toml").read_text()
    m = re.search(r'(?ms)^\[\[sources\]\]\nname = "([^"]+)"\nkind = "folder"\nroot = "([^"]+)"', text)
    if not m:
        print("no folder source in the config", file=sys.stderr)
        return 1
    source = {"name": m.group(1), "root": Path(m.group(2))}
    chosen, total = pick(rows(args.schema, source["name"]), args.chunks, args.seed,
                         args.max_doc_chunks)
    files = args.out / "files"
    files.mkdir(parents=True, exist_ok=True)
    for _, path, *_ in chosen:
        target = files / path
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            shutil.copy2(source["root"] / path, target)
    (args.out / "manifest.tsv").write_text(
        "".join(f"{d[0]}\t{d[2]}\t{d[3]}\t{d[4]:.2f}\t{d[1]}\n" for d in chosen))
    config(args.config, args.out, source, args.name)
    print(f"{len(chosen)} documents, {total} chunks in {files}; config in {args.out / 'config'}")
    return 0


def _self_check() -> None:
    docs = [(i, f"P{i % 2}/f{i}.pdf", "mail" if i % 3 else "drawing", 10, 0.9 if i % 4 else 0.1)
            for i in range(40)]
    chosen, total = pick(docs, 100, "s", 40)
    assert total >= 100 and len(chosen) == 10
    assert pick(docs, 100, "s", 40) == (chosen, total)
    assert {d[1].split("/")[0] for d in chosen} == {"P0", "P1"}
    assert {d[2] for d in chosen} == {"mail", "drawing"}


if __name__ == "__main__":
    _self_check()
    sys.exit(main())
