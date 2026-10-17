"""ytm_catalogue.py — the InnerTube request bodies and response parsers.

The `next_*` and `get_queue` fixtures are real anonymous responses captured on
2026-10-17 (trimmed to a few entries, tracking parameters removed). The signed-in
library and history shapes below are written from the documented layout, because no
signed-in jar exists in the workspace these were written in — so those tests pin the
parser's *tolerance* (renderers found wherever they are nested, sign-in placeholders
recognised), not a captured byte layout.

Run with:  python3 -m unittest discover -s firmware/pi-ytmusic/receiver/tests
"""

import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import ytm_catalogue as c  # noqa: E402


def fixture(name):
    with open(os.path.join(HERE, 'fixtures', name), encoding='utf-8') as f:
        return json.load(f)


def runs(*texts):
    return {'runs': [{'text': t} for t in texts]}


def thumb(url, size):
    return {'thumbnails': [{'url': url, 'width': size, 'height': size}]}


def two_row(title, subtitle, *, browse=None, page=None, play=None, video=None):
    item = {'title': runs(title), 'subtitle': runs(*subtitle),
            'thumbnailRenderer': {'musicThumbnailRenderer': {'thumbnail': thumb(
                'https://lh3.googleusercontent.com/x=w226-h226-l90-rj', 226)}}}
    if browse:
        item['navigationEndpoint'] = {'browseEndpoint': {
            'browseId': browse,
            'browseEndpointContextSupportedConfigs': {'browseEndpointContextMusicConfig': {'pageType': page}}}}
    if play:
        endpoint = {'watchEndpoint': {'playlistId': play, 'videoId': video}} if video else \
            {'watchPlaylistEndpoint': {'playlistId': play}}
        item['thumbnailOverlay'] = {'musicItemThumbnailOverlayRenderer': {'content': {
            'musicPlayButtonRenderer': {'playNavigationEndpoint': endpoint}}}}
    return {'musicTwoRowItemRenderer': item}


def history_song(video_id, title, artist, album, length):
    col = lambda r: {'musicResponsiveListItemFlexColumnRenderer': {'text': r}}  # noqa: E731
    return {'musicResponsiveListItemRenderer': {
        'playlistItemData': {'videoId': video_id},
        'thumbnail': {'musicThumbnailRenderer': {'thumbnail': thumb('https://lh3.googleusercontent.com/a=w60-h60-l90-rj', 60)}},
        'flexColumns': [col(runs(title)), col(runs(artist)), col({'runs': [{
            'text': album, 'navigationEndpoint': {'browseEndpoint': {
                'browseId': 'MPREb_x', 'browseEndpointContextSupportedConfigs': {
                    'browseEndpointContextMusicConfig': {'pageType': 'MUSIC_PAGE_TYPE_ALBUM'}}}}}]})],
        'fixedColumns': [{'musicResponsiveListItemFixedColumnRenderer': {'text': runs(length)}}],
    }}


def browse_page(*section_items):
    """`singleColumnBrowseResultsRenderer` → tab → sectionList → one grid per argument."""
    return {'contents': {'singleColumnBrowseResultsRenderer': {'tabs': [{'tabRenderer': {'content': {
        'sectionListRenderer': {'contents': [{'gridRenderer': {'items': list(items)}} for items in section_items]}}}}]}}}


class RequestBodies(unittest.TestCase):
    def test_a_per_song_radio_is_seeded_with_its_song(self):
        # Without the seed the server starts the mix one track later (measured).
        q = c.watch_query('RDAMVMdQw4w9WgXcQ')
        self.assertEqual(q['videoId'], 'dQw4w9WgXcQ')
        self.assertTrue(q['isAudioOnly'])

    def test_a_playlist_is_asked_without_a_seed_unless_one_is_given(self):
        self.assertNotIn('videoId', c.watch_query('PLabc'))
        self.assertEqual(c.watch_query('PLabc', 'dQw4w9WgXcQ')['videoId'], 'dQw4w9WgXcQ')

    def test_ids_are_validated_by_shape(self):
        self.assertTrue(c.is_video_id('dQw4w9WgXcQ'))
        for bad in ('dQw4w9WgXc', 'dQw4w9WgXcQQ', '../../etc/x', None, 3):
            self.assertFalse(c.is_video_id(bad), bad)
        for ok in ('LM', 'PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI', 'OLAK5uy_abc', 'RDAMVMdQw4w9WgXcQ'):
            self.assertTrue(c.is_playlist_id(ok), ok)
        for bad in ('x', 'PL abc', 'PL/abc', 'a' * 81):
            self.assertFalse(c.is_playlist_id(bad), bad)


class Queues(unittest.TestCase):
    def test_a_mix_queue_parses_titles_artists_and_a_continuation(self):
        q = c.parse_watch_queue(fixture('next_mix.json'), 'RDAMVMdQw4w9WgXcQ')
        self.assertEqual(q['playlist_id'], 'RDAMVMdQw4w9WgXcQ')
        # The panel has no title of its own; the queue header's subtitle names the list.
        self.assertEqual(q['title'], 'Never Gonna Give You Up Mix')
        self.assertTrue(q['infinite'])
        self.assertTrue(q['continuation'])
        self.assertEqual([t['video_id'] for t in q['items']], ['yPYZpwSpKmA', 'djV11Xbc914', 'Gs069dndIYk'])
        first = q['items'][0]
        self.assertEqual(first['title'], 'Together Forever (2022 Remaster)')
        # A video's byline is "Artist • views • likes": the counters are not an album.
        self.assertEqual(first['artist'], 'Rick Astley')
        self.assertIsNone(first['album'])
        self.assertEqual(first['duration'], 204)
        self.assertTrue(first['thumbnail'].startswith('https://i.ytimg.com/vi/yPYZpwSpKmA/'))
        self.assertEqual(q['items'][2]['artist'], 'Earth, Wind & Fire')

    def test_a_playlist_queue_joins_several_artists(self):
        q = c.parse_watch_queue(fixture('next_playlist.json'), 'PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI')
        self.assertEqual(q['title'], 'Popular Music Videos')
        self.assertFalse(q['infinite'])
        self.assertEqual(q['items'][0]['artist'], 'Peso Pluma & Anitta')
        self.assertEqual(q['items'][1]['duration'], 650)

    def test_get_queue_hydrates_songs_with_their_album_and_square_art(self):
        tracks = c.parse_get_queue(fixture('get_queue.json'))
        self.assertEqual([t['video_id'] for t in tracks], ['lYBUbBu4W08', 'djV11Xbc914'])
        song = tracks[0]
        self.assertEqual((song['title'], song['artist'], song['album']),
                         ('Never Gonna Give You Up', 'Rick Astley', 'Whenever You Need Somebody'))
        # googleusercontent art is re-requested large; a 60 px square is mush on a mirror.
        self.assertIn(f'=w{c.THUMB_SIZE}-h{c.THUMB_SIZE}', song['thumbnail'])

    def test_a_continuation_page_parses_the_same_way(self):
        page = {'continuationContents': {'playlistPanelContinuation': {
            'contents': fixture('next_playlist.json')['contents']['singleColumnMusicWatchNextResultsRenderer'][
                'tabbedRenderer']['watchNextTabbedResultsRenderer']['tabs'][0]['tabRenderer']['content'][
                'musicQueueRenderer']['content']['playlistPanelRenderer']['contents'],
            'continuations': [{'nextContinuationData': {'continuation': 'TOKEN2'}}]}}}
        cont = c.parse_queue_continuation(page)
        self.assertEqual(len(cont['items']), 2)
        self.assertEqual(cont['continuation'], 'TOKEN2')

    def test_a_wrapped_song_is_listed_once_by_its_primary_entry(self):
        def panel(video_id, title):
            return {'playlistPanelVideoRenderer': {'videoId': video_id, 'title': runs(title),
                                                   'longBylineText': runs('Artist'), 'lengthText': runs('3:00')}}
        response = {'queueDatas': [{'content': {'playlistPanelVideoWrapperRenderer': {
            'primaryRenderer': panel('aaaaaaaaaaa', 'Song'),
            'counterpart': [{'counterpartRenderer': panel('bbbbbbbbbbb', 'Song (Video)')}]}}}]}
        self.assertEqual([t['video_id'] for t in c.parse_get_queue(response)], ['aaaaaaaaaaa'])


class Library(unittest.TestCase):
    def test_the_signed_out_placeholder_is_recognised(self):
        lib = c.parse_library(fixture('library_signed_out.json'))
        self.assertFalse(lib['signed_in'])
        self.assertEqual(lib['items'], [])

    def test_playable_tiles_become_items_and_the_new_playlist_tile_does_not(self):
        page = browse_page([
            two_row('New playlist', []),
            two_row('Liked Music', ['Auto playlist'], browse='VLLM', page='MUSIC_PAGE_TYPE_PLAYLIST'),
            two_row('Road trip', ['Playlist', ' • ', 'David'], browse='VLPLroadtrip123',
                    page='MUSIC_PAGE_TYPE_PLAYLIST', play='PLroadtrip123'),
            two_row('Whenever You Need Somebody', ['Album', ' • ', 'Rick Astley'], browse='MPREb_abc',
                    page='MUSIC_PAGE_TYPE_ALBUM', play='OLAK5uy_album1'),
            two_row('My Supermix', ['Mix'], play='RDTMAK5uy_supermix'),
            two_row('Rick Astley', ['Artist'], browse='UCabc', page='MUSIC_PAGE_TYPE_ARTIST'),
        ])
        lib = c.parse_library(page)
        self.assertTrue(lib['signed_in'])
        self.assertEqual([(i['playlist_id'], i['kind']) for i in lib['items']], [
            ('LM', 'playlist'), ('PLroadtrip123', 'playlist'), ('OLAK5uy_album1', 'album'),
            ('RDTMAK5uy_supermix', 'mix')])
        self.assertEqual(lib['items'][1]['subtitle'], 'Playlist • David')
        self.assertIn(f'=w{c.THUMB_SIZE}-h{c.THUMB_SIZE}', lib['items'][0]['thumbnail'])

    def test_a_list_row_item_with_a_play_target_counts_too(self):
        row = {'musicResponsiveListItemRenderer': {
            'flexColumns': [{'musicResponsiveListItemFlexColumnRenderer': {'text': runs('Chill')}},
                            {'musicResponsiveListItemFlexColumnRenderer': {'text': runs('Playlist • 40 songs')}}],
            'navigationEndpoint': {'browseEndpoint': {'browseId': 'VLPLchill'}}}}
        lib = c.parse_library({'contents': [row]})
        self.assertEqual(lib['items'][0]['playlist_id'], 'PLchill')
        self.assertEqual(lib['items'][0]['subtitle'], 'Playlist • 40 songs')


class History(unittest.TestCase):
    def test_history_songs_carry_artist_album_and_length(self):
        page = {'contents': {'singleColumnBrowseResultsRenderer': {'tabs': [{'tabRenderer': {'content': {
            'sectionListRenderer': {'contents': [{'musicShelfRenderer': {'title': runs('Today'), 'contents': [
                history_song('lYBUbBu4W08', 'Never Gonna Give You Up', 'Rick Astley', 'Whenever You Need Somebody', '3:34'),
                history_song('djV11Xbc914', 'Take On Me', 'a-ha', 'Hunting High and Low', '3:46'),
            ]}}]}}}}]}}}
        hist = c.parse_history(page)
        self.assertTrue(hist['signed_in'])
        self.assertEqual([t['video_id'] for t in hist['items']], ['lYBUbBu4W08', 'djV11Xbc914'])
        self.assertEqual(hist['items'][0]['artist'], 'Rick Astley')
        self.assertEqual(hist['items'][0]['album'], 'Whenever You Need Somebody')
        self.assertEqual(hist['items'][0]['duration'], 214)

    def test_history_signed_out(self):
        self.assertFalse(c.parse_history(fixture('library_signed_out.json'))['signed_in'])


class Readers(unittest.TestCase):
    def test_durations(self):
        self.assertEqual(c.duration_seconds('3:34'), 214)
        self.assertEqual(c.duration_seconds('1:02:03'), 3723)
        self.assertIsNone(c.duration_seconds('LIVE'))
        self.assertIsNone(c.duration_seconds(None))

    def test_text_reads_runs_and_simple_text(self):
        self.assertEqual(c.text(runs('a', 'b')), 'ab')
        self.assertEqual(c.text({'simpleText': ' x '}), 'x')
        self.assertIsNone(c.text(None))


if __name__ == '__main__':
    unittest.main()
