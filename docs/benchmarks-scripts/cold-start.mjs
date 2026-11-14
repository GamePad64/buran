import { spawn } from 'node:child_process';
const N = 10;
const times = [];
for (let i = 0; i < N; i++) {
  const t0 = performance.now();
  await new Promise((res) => {
    const p = spawn('/mnt/devenv/workspace/buran/target/release/buran', ['serve'], { stdio: ['pipe','pipe','ignore'] });
    let buf = '';
    p.stdout.on('data', d => {
      buf += d;
      if (buf.includes('"id":1')) { p.kill(); res(); }
    });
    p.stdin.write(JSON.stringify({jsonrpc:'2.0',id:1,method:'initialize',params:{protocolVersion:1,clientCapabilities:{}}})+'\n');
  });
  times.push(performance.now() - t0);
}
times.sort((a,b)=>a-b);
console.log(JSON.stringify({ cold_start_ms: { p50: Math.round(times[5]), min: Math.round(times[0]), max: Math.round(times[9]) } }));
