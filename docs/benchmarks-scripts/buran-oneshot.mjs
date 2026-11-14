// One-shot: spawn buran serve, drive ACP prompt, measure wall time + RSS peak
import { spawn } from 'node:child_process';
import fs from 'node:fs';
const t0 = Date.now();
const p = spawn('/mnt/devenv/workspace/buran/target/release/buran', ['serve'], { stdio: ['pipe','pipe','inherit'] });
let buf = '', id = 0, sessionId;
const send = m => p.stdin.write(JSON.stringify(m) + '\n');
const rpc = (method, params) => new Promise((res, rej) => {
  const mine = ++id;
  const on = d => {
    buf += d; let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i); buf = buf.slice(i+1);
      if (!line.trim()) continue;
      let m; try { m = JSON.parse(line); } catch { continue; }
      if (m.id === mine) { p.stdout.off('data', on); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); }
    }
  };
  p.stdout.on('data', on);
  send({ jsonrpc: '2.0', id: mine, method, params });
});
await rpc('initialize', { protocolVersion: 1, clientCapabilities: { fs: { readTextFile: true, writeTextFile: true }, terminal: true } });
const s = await rpc('session/new', { cwd: '/tmp', mcpServers: [] });
sessionId = s.sessionId;
const t1 = Date.now();
let rssPeak = 0;
const poller = setInterval(() => {
  try { rssPeak = Math.max(rssPeak, parseInt(fs.readFileSync(`/proc/${p.pid}/status`,'utf8').match(/VmRSS:\s+(\d+)/)[1])); } catch {}
}, 30);
const stop = await rpc('session/prompt', { sessionId, prompt: [{ type: 'text', text: 'Reply with exactly one word: ping' }] });
const t2 = Date.now();
clearInterval(poller);
console.log(JSON.stringify({ handshake_ms: t1-t0, prompt_ms: t2-t1, total_ms: t2-t0, serve_rss_peak_mb: (rssPeak/1024).toFixed(1) }));
p.kill(); process.exit(0);
