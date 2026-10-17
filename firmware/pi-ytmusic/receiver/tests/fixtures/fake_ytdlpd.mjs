#!/usr/bin/env node
// Stands in for the venv's python in ytdlp.test.js: YtdlpDaemon spawns
// `<python> ytdlp_daemon.py --socket PATH ...`, so this receives the script path first and
// speaks just enough of the protocol — ping, an escalating invalidate (exit 0, as the
// real daemon does), and `crash` (exit 1). FAKE_YTDLPD_CRASH=1 exits before listening.
import fs from 'fs';
import net from 'net';

if (process.env.FAKE_YTDLPD_CRASH === '1') {
  process.exit(1);
}
const socketPath = process.argv[process.argv.indexOf('--socket') + 1];
fs.rmSync(socketPath, { force: true });
const server = net.createServer((conn) => {
  let buffer = '';
  conn.setEncoding('utf8');
  conn.on('data', (chunk) => {
    buffer += chunk;
    const lines = buffer.split('\n');
    buffer = lines.pop();
    for (const line of lines.filter(Boolean)) {
      const req = JSON.parse(line);
      if (req.op === 'ping') {
        conn.write(`${JSON.stringify({ id: req.id, version: 'fake', modes: [], ops: [] })}\n`);
      }
      else if (req.op === 'invalidate') {
        conn.write(`${JSON.stringify({ id: req.id, ok: true })}\n`);
        if (req.escalate) {
          server.close();
          setImmediate(() => process.exit(0));
        }
      }
      else if (req.op === 'crash') {
        process.exit(1);
      }
    }
  });
});
server.listen(socketPath);
