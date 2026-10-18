/**
 * Next and previous for a cast queue, from the list the phone sent.
 *
 * WHY THIS EXISTS
 * ---------------
 * yt-cast-receiver's `Playlist` asks its request handler for the neighbours of the current
 * track, and the library's `DefaultPlaylistRequestHandler` answers by asking YouTube's
 * `/next` — anonymously, as a 2023 TV client — for the autoplay set of that video *in that
 * playlist*. An anonymous request cannot see a private playlist or Liked Music (`LM`), so
 * for exactly the lists people cast from their own library the answer has no neighbours,
 * `next` stays null, and the session stops after the first song. Measured: `next: null` for
 * `LM`, while a public mix answered fine — which is why it went unnoticed.
 *
 * The phone already sent the list: the Lounge `setPlaylist` message carries the queue's
 * video ids, which the `Playlist` keeps as `videoIds`. So the neighbours come from there,
 * without a request. Only past the end of that list — where YouTube would autoplay
 * something — is the library's handler asked, and after it the receiver's own catalogue,
 * which uses the cookie jar and therefore sees private lists too.
 */

import { DefaultPlaylistRequestHandler, PlaylistRequestHandler } from 'yt-cast-receiver';

export default class CastQueueHandler extends PlaylistRequestHandler {
  #fallback;
  #catalogue;

  /**
   * @param fallback  the handler asked past the end of the phone's list (the library's,
   *                  by default), i.e. for autoplay.
   * @param catalogue catalogue.js, asked when the fallback has no next either; optional.
   */
  constructor({ fallback = new DefaultPlaylistRequestHandler(), catalogue = null } = {}) {
    super();
    this.#fallback = fallback;
    this.#catalogue = catalogue;
  }

  setLogger(logger) {
    super.setLogger(logger);
    this.#fallback.setLogger(logger);
  }

  async getPreviousNextVideos(target, playlist) {
    const ids = playlist?.videoIds ?? [];
    const at = indexOf(ids, target);
    const neighbour = (i) => (i >= 0 && i < ids.length
      ? { id: ids[i], client: target.client,
        context: { ...(playlistIdOf(playlist, target) ? { playlistId: playlistIdOf(playlist, target) } : {}), index: i } }
      : null);
    const previous = neighbour(at - 1);
    const next = at >= 0 ? neighbour(at + 1) : null;
    if (next) {
      this.logger.debug(`[castqueue] ${target.id}: next ${next.id} from the phone's list (#${at + 2} of ${ids.length})`);
      return { previous, next };
    }

    // The end of the list the phone sent, or a target that is not in it: what plays next
    // is YouTube's call (autoplay), so ask it.
    let fromFallback = { previous: null, next: null };
    try {
      fromFallback = await this.#fallback.getPreviousNextVideos(target, playlist);
    }
    catch (e) {
      this.logger.warn(`[castqueue] autoplay lookup for ${target.id} failed: ${e.message}`);
    }
    return {
      previous: previous ?? fromFallback?.previous ?? null,
      next: fromFallback?.next ?? (at < 0 ? await this.#fromCatalogue(target, playlist) : null),
    };
  }

  /**
   * The track after `target` in its playlist, as the signed-in catalogue sees it. Only
   * for a target missing from the phone's list: at the *end* of a list there is nothing
   * after it, and the catalogue would only confirm that with a request.
   */
  async #fromCatalogue(target, playlist) {
    const playlistId = playlistIdOf(playlist, target);
    if (!playlistId || !this.#catalogue?.available) {
      return null;
    }
    try {
      const page = await this.#catalogue.queue(playlistId, target.id);
      const items = page.items ?? [];
      const at = items.findIndex((t) => t.video_id === target.id);
      const after = at >= 0 ? items[at + 1] : null;
      return after ? { id: after.video_id, client: target.client, context: { playlistId, index: at + 1 } } : null;
    }
    catch (e) {
      this.logger.warn(`[castqueue] could not look up what follows ${target.id} in ${playlistId}: ${e.message}`);
      return null;
    }
  }

  reset() {
    this.#fallback.reset();
  }
}

/**
 * Where `target` is in the phone's list. Its own `context.index` decides when it agrees
 * with the list, because a playlist may hold the same song twice and `indexOf` would
 * always find the first.
 */
function indexOf(ids, target) {
  const claimed = target?.context?.index;
  if (Number.isInteger(claimed) && ids[claimed] === target.id) {
    return claimed;
  }
  return ids.indexOf(target?.id);
}

function playlistIdOf(playlist, target) {
  return target?.context?.playlistId ?? playlist?.id ?? null;
}
