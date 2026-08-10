/**
 * Scheduling priority for resolution work.
 *
 * A resolve must never compete with playback.
 *
 * Measured on the Pi Zero 2 W (2026-08-10): one resolve is `yt-dlp` at ~26 % of a
 * core *plus* the JS runtime it spawns for the `n` challenge at **>100 % and ~88 MB
 * RSS**, and because resolution is deliberately done while a track is playing, that
 * lands mid-song on a 4-core 1 GHz box with ~80 MB free. mpv and the receiver both
 * run at nice 0, so an unniced resolve takes CPU straight off the player.
 *
 * `nice` and `ionice` are applied by exec'ing *through* the wrappers rather than
 * after the fact: the priority then holds from the first instruction, and — the part
 * that matters here — it is **inherited by the JS runtime yt-dlp spawns**, which is
 * the expensive process. `--js-runtimes` gives yt-dlp a plain binary path, so there
 * is no other way to reach that child.
 *
 * The unit's `CPUWeight` cannot express this: it throttles the whole service cgroup,
 * mpv included, i.e. the player is punished together with the offender.
 *
 * Lives in its own module because two call sites need it — the long-lived resolver
 * daemon (`ytdlp.js`), and the per-track `yt-dlp` spawn that `resolver.js` still
 * falls back to when there is no daemon.
 */

import fs from 'fs';

export const RESOLVE_NICE = 19;
/** `ionice -c 3` (idle): SD-card reads for the Python/extractor import yield too. */
export const IONICE_CLASS = 3;

/**
 * Build the argv prefix that lowers a resolve's priority, using whichever wrappers
 * this box actually has. Callers fall back to `os.setPriority` when it comes back
 * empty, which covers CPU but not I/O.
 */
export function priorityPrefix() {
  const prefix = [];
  if (fs.existsSync('/usr/bin/nice')) {
    prefix.push('/usr/bin/nice', '-n', String(RESOLVE_NICE));
  }
  if (fs.existsSync('/usr/bin/ionice')) {
    prefix.push('/usr/bin/ionice', '-c', String(IONICE_CLASS));
  }
  return prefix;
}
