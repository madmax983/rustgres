#!/usr/bin/env python3
"""Fixed-iteration-count driver for a tsvector/tsquery-heavy workload.

Same rationale as profile_jsonpath.py/profile_regexp.py: bench.py-style
workloads are time-boxed, so two profiling runs execute a different amount
of work whenever wall-clock speed jitters under valgrind, confounding a
before/after instruction/allocation comparison. This driver does a fixed
amount of work instead:

  1. INDEX phase: insert --rows documents (realistic multi-sentence
     article bodies, not single-word toys), each row computing
     `to_tsvector('english', body)` inline -- the textbook "build a search
     index column at write time" usage from the v1.94 docs, exercising the
     tokenizer + stopword filter + Snowball stemmer + tsvector builder on
     real-shaped text (mixed stopwords, repeated and inflected word forms,
     punctuation, numbers).
  2. SEARCH phase: --count exact iterations of a multi-term ranked search
     (`... WHERE tsv @@ to_tsquery(...) ORDER BY ts_rank(tsv, query) DESC
     LIMIT 10`), the textbook ranked full-text search query, over the real
     wire protocol.

Usage:
    cargo build && ./target/debug/rustgres &                 # terminal 1
    python3 benches/profile_tsearch.py --rows 2000 --count 30 # terminal 2
    # or under valgrind, same as benches/profile.sh:
    valgrind --tool=callgrind ./target/debug/rustgres &
    python3 benches/profile_tsearch.py --rows 2000 --count 30 --timeout 1800
"""
import argparse
import os
import random
import sys

sys.path.insert(0, os.path.dirname(__file__))
from bench import Conn, tag_of  # noqa: E402

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 5433

# A vocabulary mixing common stopwords, plain words, and several words in
# multiple inflections (so the Snowball stemmer and stopword filter both do
# real work, not just dictionary lookups on a handful of distinct forms).
STOPWORDS = [
    "the", "a", "an", "of", "in", "on", "and", "or", "to", "for", "with",
    "is", "are", "was", "were", "it", "its", "this", "that", "by", "as",
    "at", "from", "be", "has", "have", "had",
]
CONTENT_WORDS = [
    "run", "running", "runs", "ran", "runner",
    "connect", "connected", "connecting", "connection", "connections",
    "nation", "national", "nationally", "nationalize",
    "happy", "happiness", "happily", "unhappy",
    "fish", "fishing", "fisher", "fishes",
    "database", "databases", "transaction", "transactions", "query",
    "queries", "index", "indexes", "indexing", "indexed",
    "server", "servers", "client", "clients", "network", "networks",
    "networking", "protocol", "protocols", "storage", "stored", "storing",
    "cluster", "clusters", "replicate", "replication", "replicated",
    "optimize", "optimizer", "optimization", "optimized",
    "lexeme", "lexemes", "token", "tokens", "tokenize", "tokenizer",
    "document", "documents", "ranking", "ranked", "rank", "search",
    "searching", "searches", "searched", "vector", "vectors", "parser",
    "parsing", "parsed", "engine", "engines", "write", "writing", "written",
    "read", "reading", "reader", "developer", "developers", "developing",
    "system", "systems", "performance", "benchmark", "benchmarking",
]
VOCAB = STOPWORDS + CONTENT_WORDS


def doc_body(i, rng, words_per_doc=60):
    """One realistic-shaped article body: several sentences, punctuation,
    a number, mixed-case leading words -- not a bag of lowercase tokens."""
    words = rng.choices(VOCAB, k=words_per_doc)
    sentences = []
    pos = 0
    while pos < len(words):
        n = rng.randint(6, 14)
        chunk = words[pos:pos + n]
        pos += n
        if not chunk:
            continue
        chunk[0] = chunk[0].capitalize()
        sentences.append(" ".join(chunk) + ".")
    body = " ".join(sentences)
    return f"Report {i}, revision {i % 7}: {body} See section {i % 20} for details."


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default=DEFAULT_HOST)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--rows", type=int, default=600,
                     help="documents in bench_tsearch (default 600)")
    ap.add_argument("--count", type=int, default=15,
                     help="exact number of ranked-search query iterations")
    ap.add_argument("--timeout", type=float, default=1800.0,
                     help="socket timeout in seconds (valgrind is slow; "
                          "bench.py's default 30s is too short here)")
    args = ap.parse_args()

    rng = random.Random(42)

    conn = Conn(args.host, args.port)
    conn.s.settimeout(args.timeout)
    conn.simple("DROP TABLE IF EXISTS bench_tsearch")
    r = conn.simple(
        "CREATE TABLE bench_tsearch(id INT, body TEXT, tsv TSVECTOR)"
    )
    assert tag_of(r) == "CREATE TABLE", r

    # INDEX phase: insert documents, computing to_tsvector inline (the
    # realistic "build the search column at write time" path).
    batch = 100
    for j in range(0, args.rows, batch):
        n = min(batch, args.rows - j)
        vals = ",".join(
            "({0},'{1}',to_tsvector('english','{1}'))".format(
                i, doc_body(i, rng).replace("'", "''")
            )
            for i in range(j, j + n)
        )
        r = conn.simple(f"INSERT INTO bench_tsearch VALUES {vals}")
        assert tag_of(r) == f"INSERT 0 {n}", tag_of(r)

    # SEARCH phase: exact count of a ranked multi-term search, the
    # textbook tsvector/tsquery query shape.
    sql = (
        "SELECT id, ts_rank(tsv, to_tsquery('english', "
        "'database & (query | connection) & !fishing')) AS r "
        "FROM bench_tsearch "
        "WHERE tsv @@ to_tsquery('english', "
        "'database & (query | connection) & !fishing') "
        "ORDER BY r DESC LIMIT 10"
    )

    for _ in range(args.count):
        msgs = conn.simple(sql)
        tag = tag_of(msgs)
        assert tag is not None and tag.startswith("SELECT"), (tag, msgs)
    print(
        f"indexed {args.rows} documents (to_tsvector inline), then ran "
        f"{args.count} iterations of a ranked tsvector/tsquery search",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
