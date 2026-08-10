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
import re
import sys
from urllib.request import Request, urlopen
from urllib.error import HTTPError

FORUM_BASE = "https://mozaiklabs.fr/api/v1/forum"
GITHUB_API = "https://api.github.com"

# Label d'exemption : une issue qui le porte n'est jamais refermée automatiquement.
KEEP_OPEN_LABEL = "keep-open"

FORUM_TOKEN = os.environ["FORUM_TOKEN"]
GITHUB_TOKEN = os.environ["GITHUB_TOKEN"]
GIST_ID = os.environ.get("GIST_ID", "")
GIST_TOKEN = os.environ.get("GIST_TOKEN", GITHUB_TOKEN)
REPO = os.environ["GITHUB_REPOSITORY"]


def forum_thread_url(slug):
    """URL publique d'un fil du forum.

    Le segment est « threads » au PLURIEL : `/forum/thread/{slug}` renvoie 404.
    Le script a écrit la forme au singulier pendant des mois — le champ « Lien »
    de toutes les issues [Forum] créées jusqu'au 2026-08-09 pointe donc vers une
    page inexistante. Passer par cette fonction plutôt que de recomposer l'URL.
    """
    return f"https://mozaiklabs.fr/forum/threads/{slug}"


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


def list_open_forum_issues():
    """Issues ouvertes portant le label `forum-feedback`, page par page."""
    out, page = [], 1
    while True:
        batch = http_get(
            f"{GITHUB_API}/repos/{REPO}/issues"
            f"?state=open&labels=forum-feedback&per_page=100&page={page}",
            {"Authorization": f"token {GITHUB_TOKEN}", "Accept": "application/vnd.github+json"},
        )
        if not batch:
            break
        # L'API `issues` renvoie aussi les pull requests : on les écarte.
        out.extend(i for i in batch if "pull_request" not in i)
        if len(batch) < 100:
            break
        page += 1
    return out


def close_issue(number, comment):
    http_post(
        f"{GITHUB_API}/repos/{REPO}/issues/{number}/comments",
        {"body": comment},
        {"Authorization": f"token {GITHUB_TOKEN}", "Accept": "application/vnd.github+json"},
    )
    http_patch(
        f"{GITHUB_API}/repos/{REPO}/issues/{number}",
        {"state": "closed", "state_reason": "completed"},
        {"Authorization": f"token {GITHUB_TOKEN}", "Accept": "application/vnd.github+json"},
    )


def close_issues_for_resolved_threads(threads):
    """Ferme les issues dont le fil forum est passé à `resolved`.

    Sans cela, le dépôt accumule indéfiniment : le watcher crée une issue par
    fil mais rien ne la referme jamais. Au 2026-08-10, **309 des 563 issues
    ouvertes** avaient leur fil déjà résolu — plus d'une sur deux ne
    correspondait à rien de vivant, et le backlog en devenait inexploitable.

    Seul `resolved` ferme. Un fil `closed` reste ouvert ici volontairement :
    c'est le statut posé quand on renvoie le suivi vers GitHub en disant au
    testeur « le sujet n'est pas réglé, il vit désormais dans l'issue » —
    la fermer trahirait la promesse faite.

    Le label `keep-open` exempte une issue de cette passe. Sans lui, aucune
    décision humaine ne survivait : garder délibérément une issue ouverte alors
    que son fil est résolu — le temps d'une relecture, ou parce que le défaut
    persiste malgré ce qu'en pense l'auteur du fil — était défait à l'exécution
    suivante, en silence et sans recours.
    """
    resolved = {
        t.get("slug"): t
        for t in threads
        if t.get("slug") and t.get("status") == "resolved"
    }
    if not resolved:
        return 0
    closed = 0
    for issue in list_open_forum_issues():
        labels = {l.get("name") for l in issue.get("labels") or []}
        if KEEP_OPEN_LABEL in labels:
            continue
        m = re.search(r"forum/thread[s]?/([A-Za-z0-9\-]+)", issue.get("body") or "")
        if not m or m.group(1) not in resolved:
            continue
        try:
            close_issue(
                issue["number"],
                "Fermeture automatique : le fil forum d'origine est passé à "
                "**résolu**.\n\nSi le défaut existe encore, rouvrez cette issue "
                "ou ouvrez un nouveau fil — le forum reste la porte d'entrée.",
            )
            print(f"Closed issue #{issue['number']} (thread resolved)")
            closed += 1
        except Exception as e:
            print(f"Failed to close issue #{issue['number']}: {e}", file=sys.stderr)
    return closed


def strip_html(text):
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
            f"**Lien** : {forum_thread_url(t.get('slug',''))}\n\n"
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
            f"**Lien** : {forum_thread_url(t.get('slug',''))}\n\n"
            f"**Contenu** :\n\n> {strip_html(r.get('body',''))[:500]}"
        )
        title = f"[Forum reply] {t.get('title','(no title)')[:60]}"
        try:
            issue = create_github_issue(title, body, ["forum-feedback", "new-reply"])
            print(f"Created issue #{issue['number']}: {title}")
            created += 1
        except Exception as e:
            print(f"Failed to create issue for reply: {e}", file=sys.stderr)

    # Refermer ce qui a été résolu côté forum, sinon le dépôt accumule.
    try:
        closed = close_issues_for_resolved_threads(threads)
    except Exception as e:
        closed = 0
        print(f"close pass failed: {e}", file=sys.stderr)

    state = {"last_thread_id": max_thread_id, "thread_reply_counts": new_reply_counts}
    save_state(state)

    print(f"Done. {created} new issue(s) created, {closed} closed.")


if __name__ == "__main__":
    main()
