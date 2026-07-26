/*
 * LOCAL SHIM (pipewire_audio_router) — not part of upstream libairptp.
 *
 * libairptp exposes three optional callbacks (airptp.h `struct airptp_callbacks`)
 * that its worker thread invokes: thread_name_set, hexdump, logmsg. They are
 * thread-local (`__thread airptp_cb`), and the worker inherits them from whichever
 * thread calls airptp_daemon_start (it copies the caller's TLS into the daemon and
 * re-registers on the worker). So we install them from Rust's start thread, just
 * before airptp_daemon_start, via bridge_airptp_install_callbacks().
 *
 * Purpose:
 *  - thread_name_set: name the worker "libairptp" so it's identifiable in
 *    `ps`/`/proc/<tid>/comm`/monitoring (it otherwise inherits the spawning
 *    thread's comm, e.g. "ap2-discovery").
 *  - logmsg: route libairptp's diagnostics (bind errors, peer add/remove, etc.)
 *    into the daemon's `tracing` — without this they are silently discarded.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stddef.h>  /* size_t — airptp.h uses it but doesn't include this */
#include <stdbool.h> /* bool   — ditto; older (cross) gcc isn't C23 */
#include <pthread.h>

#include "../airptp.h"

/* Implemented in Rust (src/ap2_ptp.rs) — forwards a finished line to `tracing`. */
extern void bridge_airptp_log(const char *msg);

static void
shim_thread_name_set(const char *name)
{
#if defined(__linux__)
  /* Names the calling (worker) thread; truncated to 15 chars by the kernel. */
  pthread_setname_np(pthread_self(), name);
#else
  (void)name;
#endif
}

/* libairptp calls logmsg as logmsg("%s", already_formatted_line); we re-render
 * with vsnprintf so a direct fmt+args call would also be handled correctly. */
static void
shim_logmsg(const char *fmt, ...)
{
  char buf[2048];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(buf, sizeof(buf), fmt, ap);
  va_end(ap);
  bridge_airptp_log(buf);
}

/* Call on the SAME thread that then calls airptp_daemon_start, before it. */
void
bridge_airptp_install_callbacks(void)
{
  struct airptp_callbacks cb;
  cb.thread_name_set = shim_thread_name_set;
  cb.hexdump = NULL;
  cb.logmsg = shim_logmsg;
  airptp_callbacks_register(&cb);
}
