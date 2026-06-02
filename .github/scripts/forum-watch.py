#!/usr/bin/env python3
"""
Forum watcher — poll mozaiklabs.fr forum and create GitHub issues for new
tester posts and replies.

State persistence: a gist stores the highest thread.id and a per-thread
last-reply-id map between runs.

Required env vars:
- FORUM_TOKEN: bearer token for mozaiklabs forum API
- GITHUB_TOKEN: GitHub token with repo:write scope (provided by Actions)
- GIST_ID: id of the gist used for state (created on first run)
- GIST_TOKEN: PAT with gist scope (separate from GITHUB_TOKEN)
- GITHUB_REPOSITORY: owner/repo (provided by Actions, e.g. renesenses/tune-server-rust)
"""
import json
import os
import sys
from urllib.request import Request, urlopen
from urllib.error import HTTPError

FORUM_BASE = "https://mozaiklabs.fr/api/v1/forum"
GITHUB_API = "https://api.github.com"

FORUM_TOKEN = os.environ["FORUM_TOKEN"]
GITHUB_TOKEN = os.environ["GITHUB_TOKEN"]
GIST_ID = os.environ.get("GIST_ID", "")
GIST_TOKEN = os.environ.get("GIST_TOKEN", GITHUB_TOKEN)
REPO = os.environ["GITHUB_REPOSITORY"]


def http_get(url, headers=None):
    req = Request(url, headers=headers or {})
    with urlopen(req, timeout=30) as resp:
        return json.load(resp)


def http_post(url, data, headers=None):
    body = json.dumps(data).encode()
    h = {"Content-Type": "application/json"}
    if headers:
        h.update(headers)
    req = Request(url, data=body, headers=h, method="POST")
    try:
        with urlopen(req, timeout=30) as resp:
            return json.load(resp)
    except HTTPError as e:
        print(f"HTTP {e.code}: {e.read().decode()[:500]}", file=sys.stderr)
        raise


def http_patch(url, data, headers=None):
    body = json.dumps(data).encode()
    h = {"Content-Type": "application/json"}
    if headers:
        h.update(headers)
    req = Request(url, data=body, headers=h, method="PATCH")
    with urlopen(req, timeout=30) as resp:
        return json.load(resp)


def load_state():
    if not GIST_ID:
        return {"last_thread_id": 0, "thread_reply_counts": {}}
    gist = http_get(
        f"{GITHUB_API}/gists/{GIST_ID}",
        {"Authorization": f"token {GIST_TOKEN}", "Accept": "application/vnd.github+json"},
    )
    content = gist["files"].get("forum-state.json", {}).get("content", "{}")
    try:
        return json.loads(content)
    except json.JSONDecodeError:
        return {"last_thread_id": 0, "thread_reply_counts": {}}


def save_state(state):
    if not GIST_ID:
        print("WARNING: GIST_ID not set, state not persisted", file=sys.stderr)
        return
    http_patch(
        f"{GITHUB_API}/gists/{GIST_ID}",
        {"files": {"forum-state.json": {"content": json.dumps(state, indent=2)}}},
        {"Authorization": f"token {GIST_TOKEN}", "Accept": "application/vnd.github+json"},
    )


def list_threads():
    return http_get(
        f"{FORUM_BASE}/threads",
        {"Authorization": f"Bearer {FORUM_TOKEN}"},
    ).get("threads", [])


def get_thread(slug):
    return http_get(
        f"{FORUM_BASE}/threads/{slug}",
        {"Authorization": f"Bearer {FORUM_TOKEN}"},
    )


def create_github_issue(title, body, labels):
    return http_post(
        f"{GITHUB_API}/repos/{REPO}/issues",
        {"title": title, "body": body, "labels": labels},
        {"Authorization": f"token {GITHUB_TOKEN}", "Accept": "application/vnd.github+json"},
    )


def strip_html(text):
    import re
    text = re.sub(r"<[^>]+>", " ", text)
    text = re.sub(r"\s+", " ", text)
    return text.strip()


def main():
    state = load_state()
    last_thread_id = state.get("last_thread_id", 0)
    reply_counts = state.get("thread_reply_counts", {})

    threads = list_threads()
    new_threads = []
    new_replies = []

    max_thread_id = last_thread_id
    new_reply_counts = dict(reply_counts)

    for t in threads:
        tid = t.get("id", 0)
        author = t.get("author", "")
        if author == "Admin":
            continue

        # New thread?
        if tid > last_thread_id:
            new_threads.append(t)
            max_thread_id = max(max_thread_id, tid)

        # New replies?
        slug = t.get("slug", "")
        if not slug:
            continue
        try:
            detail = get_thread(slug)
            replies = detail.get("replies", [])
            current_count = len(replies)
            known_count = reply_counts.get(slug, 0)
            if current_count > known_count:
                for r in replies[known_count:]:
                    if r.get("author") != "Admin":
                        new_replies.append((t, r))
            new_reply_counts[slug] = current_count
        except Exception as e:
            print(f"Could not fetch {slug}: {e}", file=sys.stderr)

    # Create GitHub issues
    created = 0
    for t in new_threads:
        body = (
            f"**Auteur** : {t.get('author','?')}\n"
            f"**Date** : {t.get('created_at','?')}\n"
            f"**Lien** : https://mozaiklabs.fr/forum/thread/{t.get('slug','')}\n\n"
            f"**Extrait** :\n\n> {strip_html(t.get('body',''))[:500]}"
        )
        title = f"[Forum] {t.get('title','(no title)')}"
        try:
            issue = create_github_issue(title, body, ["forum-feedback", "new-thread"])
            print(f"Created issue #{issue['number']}: {title}")
            created += 1
        except Exception as e:
            print(f"Failed to create issue for thread {t.get('id')}: {e}", file=sys.stderr)

    for t, r in new_replies:
        body = (
            f"**Réponse de** : {r.get('author','?')}\n"
            f"**Sur le thread** : {t.get('title','(no title)')}\n"
            f"**Date** : {r.get('created_at','?')}\n"
            f"**Lien** : https://mozaiklabs.fr/forum/thread/{t.get('slug','')}\n\n"
            f"**Contenu** :\n\n> {strip_html(r.get('body',''))[:500]}"
        )
        title = f"[Forum reply] {t.get('title','(no title)')[:60]}"
        try:
            issue = create_github_issue(title, body, ["forum-feedback", "new-reply"])
            print(f"Created issue #{issue['number']}: {title}")
            created += 1
        except Exception as e:
            print(f"Failed to create issue for reply: {e}", file=sys.stderr)

    state = {"last_thread_id": max_thread_id, "thread_reply_counts": new_reply_counts}
    save_state(state)

    print(f"Done. {created} new GitHub issue(s) created.")


if __name__ == "__main__":
    main()
