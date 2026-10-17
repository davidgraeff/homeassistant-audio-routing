"""YouTube Music catalogue reads: queues, track metadata, the library, the history.

WHY THIS EXISTS
---------------
The Lounge plane is a control plane of *ids*. A cast session tells the receiver "play
video X from list Y" and nothing about what X is called, and without a phone there is
no session at all — so starting a playlist from Home Assistant, showing the queue, or
naming the artist of the current song all need the same thing the YouTube Music app
itself fetches: the queue for a list, and the metadata for a handful of ids.

These are the InnerTube requests the music.youtube.com web client makes, asked with
the receiver's own cookie jar through yt-dlp's request machinery (`YoutubeBaseInfoExtractor
._extract_response`, which also computes the SAPISIDHASH authorization from the jar):

    next             {playlistId[, videoId]}   the queue YTM would play for a list —
                                               playlists, albums, Liked Music and radio
                                               mixes alike, ~25-50 items a page
    next             {continuation}            the next page of that queue
    music/get_queue  {videoIds: [...]}         metadata for ids a phone queued
    browse           FEmusic_library_landing   the library, in YTM's "recent activity" order
    browse           FEmusic_history           recently played songs

Measured anonymously (2026-10-17): `next` returns 50 items for a mix and for a public
playlist, with clean titles, artists and album art, and a working continuation;
`get_queue` hydrates three ids in one request. The two `browse` ids need a signed-in
jar and answer anonymously with a "Sign in" message, which this module recognises —
their *signed-in* layout is parsed from the documented shape and has not been seen live
from this workspace, so the parser walks for the renderers it understands rather than
indexing a fixed path.

This module is pure: it builds request bodies and parses responses, and imports nothing
from yt-dlp, so its tests run anywhere. The requests themselves are made by
`ytdlp_daemon.py`, which owns the one shared cookie jar — a second process asking with
the same session is exactly the jar-rotation hazard `push_cookies.py` warns about.
"""

from __future__ import annotations

import re

VIDEO_ID_RE = re.compile(r'^[A-Za-z0-9_-]{11}$')
#: Playlist ids seen in the wild: PL… (user), OLAK5uy_… (album), RD… (mixes and radios),
#: LM (Liked Music), VL-prefixed browse ids, SE/FL/LL. Length is the only sane bound.
PLAYLIST_ID_RE = re.compile(r'^[A-Za-z0-9_-]{2,80}$')
#: Library feeds the daemon may be asked for, by name -> InnerTube browse id. A closed
#: set, so a request can never name an arbitrary browse page.
LIBRARY_FEEDS = {
    'library': 'FEmusic_library_landing',
    'playlists': 'FEmusic_liked_playlists',
    'history': 'FEmusic_history',
}
#: Thumbnails YTM serves from googleusercontent take their size in the URL; the queue
#: carries 60-120 px ones, which look like mush on a mirror.
THUMB_SIZE = 544
_GUC_SIZE_RE = re.compile(r'=w\d+-h\d+')
_SEPARATOR = ' • '


def is_video_id(value) -> bool:
    return isinstance(value, str) and bool(VIDEO_ID_RE.match(value))


def is_playlist_id(value) -> bool:
    return isinstance(value, str) and bool(PLAYLIST_ID_RE.match(value))


# --- request bodies ---------------------------------------------------------------------

def watch_query(playlist_id: str, video_id: str | None = None) -> dict:
    """The `next` body for "the queue of this list", as the YTM web client sends it.

    A per-song radio (`RDAMVM<id>`) is seeded by that song: without the `videoId` the
    server starts the mix one track later (measured), so the seed is passed explicitly.
    """
    query = {
        'enablePersistentPlaylistPanel': True,
        'isAudioOnly': True,
        'tunerSettingValue': 'AUTOMIX_SETTING_NORMAL',
        'playlistId': playlist_id,
    }
    if video_id is None and playlist_id.startswith('RDAMVM') and is_video_id(playlist_id[6:]):
        video_id = playlist_id[6:]
    if video_id:
        query['videoId'] = video_id
    return query


def continuation_query(playlist_id: str, token: str) -> dict:
    return {
        'enablePersistentPlaylistPanel': True,
        'isAudioOnly': True,
        'playlistId': playlist_id,
        'continuation': token,
    }


def radio_playlist_id(video_id: str) -> str:
    """The radio YTM starts when a single song is tapped."""
    return f'RDAMVM{video_id}'


# --- small readers ----------------------------------------------------------------------

def text(obj) -> str | None:
    """`{runs: [{text}...]}` or `{simpleText}` -> one string."""
    if not isinstance(obj, dict):
        return None
    if isinstance(obj.get('simpleText'), str):
        return obj['simpleText'].strip() or None
    runs = obj.get('runs')
    if isinstance(runs, list):
        joined = ''.join(r.get('text', '') for r in runs if isinstance(r, dict)).strip()
        return joined or None
    return None


def duration_seconds(value) -> int | None:
    """'3:34' / '1:02:03' -> seconds."""
    if not isinstance(value, str) or not re.fullmatch(r'\d+(:\d{1,2}){0,2}', value.strip()):
        return None
    total = 0
    for part in value.strip().split(':'):
        total = total * 60 + int(part)
    return total


def best_thumbnail(container) -> str | None:
    """The largest thumbnail URL in a `{thumbnails: [...]}` (wherever it is nested).

    googleusercontent art is re-requested at THUMB_SIZE; i.ytimg URLs are left alone,
    their size is part of the path.
    """
    thumbs = _find_first(container, 'thumbnails')
    if not isinstance(thumbs, list) or not thumbs:
        return None
    best = max((t for t in thumbs if isinstance(t, dict) and t.get('url')),
               key=lambda t: (t.get('width') or 0) * (t.get('height') or 0), default=None)
    if not best:
        return None
    url = best['url']
    if url.startswith('//'):
        url = 'https:' + url
    if 'googleusercontent.com' in url:
        url = _GUC_SIZE_RE.sub(f'=w{THUMB_SIZE}-h{THUMB_SIZE}', url)
    return url


def _find_first(obj, key):
    """Depth-first: the first value stored under `key` anywhere in `obj`."""
    if isinstance(obj, dict):
        if key in obj:
            return obj[key]
        for v in obj.values():
            found = _find_first(v, key)
            if found is not None:
                return found
    elif isinstance(obj, list):
        for v in obj:
            found = _find_first(v, key)
            if found is not None:
                return found
    return None


def _walk(obj, keys: tuple[str, ...]):
    """Every dict stored under one of `keys`, in document order, not descending into a
    match (a renderer's own menu can contain renderers of the same kind)."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k in keys and isinstance(v, dict):
                yield k, v
            else:
                yield from _walk(v, keys)
    elif isinstance(obj, list):
        for v in obj:
            yield from _walk(v, keys)


def _page_type(run: dict) -> str | None:
    return (((run.get('navigationEndpoint') or {}).get('browseEndpoint') or {})
            .get('browseEndpointContextSupportedConfigs', {})
            .get('browseEndpointContextMusicConfig', {}).get('pageType'))


def _byline(runs: list) -> tuple[str | None, str | None]:
    """(artist, album) from a byline's runs.

    Songs read "Artist • Album • 1987", videos "Artist • 1.8B views • 19M likes". The
    album is recognised by its link (page type ALBUM), falling back to the song shape —
    three groups ending in a year.
    """
    groups: list[list[dict]] = [[]]
    for r in runs or []:
        if not isinstance(r, dict):
            continue
        if r.get('text') == _SEPARATOR:
            groups.append([])
        else:
            groups[-1].append(r)
    artist = ''.join(r.get('text', '') for r in groups[0]).strip() or None
    album = None
    for g in groups[1:]:
        if any(_page_type(r) == 'MUSIC_PAGE_TYPE_ALBUM' for r in g):
            album = ''.join(r.get('text', '') for r in g).strip() or None
            break
    if album is None and len(groups) == 3:
        last = ''.join(r.get('text', '') for r in groups[2]).strip()
        if re.fullmatch(r'\d{4}', last):
            album = ''.join(r.get('text', '') for r in groups[1]).strip() or None
    return artist, album


# --- tracks -----------------------------------------------------------------------------

def parse_panel_video(v: dict) -> dict | None:
    """A `playlistPanelVideoRenderer` (a queue entry) -> a track, or None."""
    video_id = v.get('videoId') or (((v.get('navigationEndpoint') or {}).get('watchEndpoint') or {}).get('videoId'))
    if not is_video_id(video_id):
        return None
    artist, album = _byline(((v.get('longBylineText') or {}).get('runs')) or ((v.get('shortBylineText') or {}).get('runs')) or [])
    return {
        'video_id': video_id,
        'title': text(v.get('title')),
        'artist': artist,
        'album': album,
        'duration': duration_seconds(text(v.get('lengthText'))),
        'thumbnail': best_thumbnail(v.get('thumbnail')),
    }


def panel_tracks(response) -> list[dict]:
    """Every queue entry in a `next` / `get_queue` / continuation response, in order.

    `playlistPanelVideoWrapperRenderer` is how YTM wraps a song that has a music-video
    counterpart; its primary renderer is the entry that plays, and because the walk
    does not descend into a match, the counterpart is never listed as a second entry.
    """
    out = []
    for kind, node in _walk(response, ('playlistPanelVideoRenderer', 'playlistPanelVideoWrapperRenderer')):
        if kind == 'playlistPanelVideoWrapperRenderer':
            node = _find_first(node.get('primaryRenderer') or {}, 'playlistPanelVideoRenderer') or {}
        track = parse_panel_video(node)
        if track:
            out.append(track)
    return out


def _continuation_token(obj) -> str | None:
    conts = _find_first(obj, 'continuations')
    if isinstance(conts, list):
        for c in conts:
            if not isinstance(c, dict):
                continue
            for k in ('nextRadioContinuationData', 'nextContinuationData'):
                tok = (c.get(k) or {}).get('continuation')
                if isinstance(tok, str) and tok:
                    return tok
    return None


def parse_watch_queue(response: dict, playlist_id: str) -> dict:
    """A `next` response -> {playlist_id, title, items, continuation, infinite}."""
    panel = _find_first(response, 'playlistPanelRenderer') or {}
    header = _find_first(response, 'musicQueueHeaderRenderer') or {}
    title = text(panel.get('title')) or text(header.get('subtitle'))
    return {
        'playlist_id': panel.get('playlistId') or playlist_id,
        'title': title,
        'items': panel_tracks(panel) if panel else panel_tracks(response),
        'continuation': _continuation_token(panel),
        'infinite': bool(panel.get('isInfinite')),
    }


def parse_queue_continuation(response: dict) -> dict:
    """A continuation of `next` -> {items, continuation}."""
    cont = _find_first(response, 'playlistPanelContinuation') or {}
    return {'items': panel_tracks(cont), 'continuation': _continuation_token(cont)}


def parse_get_queue(response: dict) -> list[dict]:
    """A `music/get_queue` response -> tracks, in the order the ids were asked."""
    return panel_tracks(response.get('queueDatas') or [])


# --- the library and the history (signed in only) ---------------------------------------

def signed_out(response) -> bool:
    """Whether YouTube answered with its "Sign in to see this" placeholder."""
    return _find_first(response, 'signInEndpoint') is not None


def _play_target(item: dict) -> tuple[str | None, str | None]:
    """(playlist_id, video_id) an item's play button would start."""
    overlay = _find_first(item.get('thumbnailOverlay') or item.get('overlay') or {}, 'playNavigationEndpoint') or {}
    for ep in (overlay, item.get('navigationEndpoint') or {}):
        wpe = ep.get('watchPlaylistEndpoint') or {}
        if is_playlist_id(wpe.get('playlistId')):
            return wpe['playlistId'], None
        we = ep.get('watchEndpoint') or {}
        if is_playlist_id(we.get('playlistId')):
            return we['playlistId'], we.get('videoId') if is_video_id(we.get('videoId')) else None
    browse = ((item.get('navigationEndpoint') or {}).get('browseEndpoint') or {}).get('browseId') or ''
    if browse.startswith('VL') and is_playlist_id(browse[2:]):
        return browse[2:], None
    return None, None


def _kind_of(item: dict, playlist_id: str) -> str:
    page = (((item.get('navigationEndpoint') or {}).get('browseEndpoint') or {})
            .get('browseEndpointContextSupportedConfigs', {})
            .get('browseEndpointContextMusicConfig', {}).get('pageType')) or ''
    if 'ALBUM' in page or playlist_id.startswith('OLAK5uy_'):
        return 'album'
    if 'ARTIST' in page:
        return 'artist'
    if playlist_id.startswith('RD'):
        return 'mix'
    return 'playlist'


def parse_library(response: dict) -> dict:
    """A library browse -> {signed_in, items: [{playlist_id, title, subtitle, kind, thumbnail}]}.

    Anything with a play target counts — playlists, albums, mixes, an artist's shuffle —
    because "what did I play recently" in YTM's library is exactly that mix. Items with
    no play target (the "New playlist" tile) are skipped.
    """
    items = []
    seen = set()
    for _, node in _walk(response, ('musicTwoRowItemRenderer', 'musicResponsiveListItemRenderer')):
        if 'flexColumns' in node:
            title = text(((node['flexColumns'][0] if node['flexColumns'] else {})
                          .get('musicResponsiveListItemFlexColumnRenderer') or {}).get('text'))
            subtitle = ' '.join(filter(None, (
                text((c.get('musicResponsiveListItemFlexColumnRenderer') or {}).get('text'))
                for c in node['flexColumns'][1:])))
        else:
            title = text(node.get('title'))
            subtitle = text(node.get('subtitle'))
        playlist_id, _ = _play_target(node)
        if not playlist_id or not title or playlist_id in seen:
            continue
        seen.add(playlist_id)
        items.append({
            'playlist_id': playlist_id,
            'title': title,
            'subtitle': subtitle or None,
            'kind': _kind_of(node, playlist_id),
            'thumbnail': best_thumbnail(node.get('thumbnailRenderer') or node.get('thumbnail')),
        })
    return {'signed_in': bool(items) or not signed_out(response), 'items': items}


def parse_history(response: dict) -> dict:
    """`FEmusic_history` -> {signed_in, items: tracks}, newest first as YTM lists them."""
    items = []
    for _, node in _walk(response, ('musicResponsiveListItemRenderer',)):
        cols = [((c or {}).get('musicResponsiveListItemFlexColumnRenderer') or {}).get('text') or {}
                for c in node.get('flexColumns') or []]
        if not cols:
            continue
        video_id = ((node.get('playlistItemData') or {}).get('videoId')
                    or ((((cols[0].get('runs') or [{}])[0]).get('navigationEndpoint') or {}).get('watchEndpoint') or {}).get('videoId')
                    or _find_first(node.get('overlay') or {}, 'videoId'))
        if not is_video_id(video_id):
            continue
        byline_runs = []
        for c in cols[1:]:
            if byline_runs:
                byline_runs.append({'text': _SEPARATOR})
            byline_runs.extend(c.get('runs') or [])
        artist, album = _byline(byline_runs)
        fixed = (((node.get('fixedColumns') or [{}])[0]).get('musicResponsiveListItemFixedColumnRenderer') or {}).get('text')
        items.append({
            'video_id': video_id,
            'title': text(cols[0]),
            'artist': artist,
            'album': album,
            'duration': duration_seconds(text(fixed)),
            'thumbnail': best_thumbnail(node.get('thumbnail')),
        })
    return {'signed_in': bool(items) or not signed_out(response), 'items': items}
